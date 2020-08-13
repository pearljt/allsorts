#![deny(missing_docs)]

//! Bitmap fonts in `EBLC`/`EBDT` and `CBLC`/`CBDT` tables.

use std::convert::{TryFrom, TryInto};
use std::fmt;

use bitreader::{BitReader, BitReaderError};

use super::BitDepth;
use crate::binary::read::{
    CheckIndex, ReadArray, ReadBinary, ReadBinaryDep, ReadCtxt, ReadFixedSizeDep, ReadFrom,
    ReadScope,
};
use crate::binary::{U16Be, U32Be, I8, U8};
use crate::bitmap::{
    Bitmap, BitmapGlyph, BitmapMetrics, EmbeddedBitmap, EmbeddedMetrics, EncapsulatedBitmap,
    EncapsulatedFormat, Metrics, OriginOffset,
};
use crate::error::ParseError;
use crate::size;

/// Flag in `BitmapInfo` `flags` indicating the direction of small glyph metrics is horizontal.
///
/// https://docs.microsoft.com/en-us/typography/opentype/spec/eblc#bitmap-flags
const HORIZONTAL_METRICS: i8 = 1;

/// Flag in `BitmapInfo` `flags` indicating the direction of small glyph metrics is vertical.
///
/// https://docs.microsoft.com/en-us/typography/opentype/spec/eblc#bitmap-flags
const VERTICAL_METRICS: i8 = 2;

/// `CBLC` — Color Bitmap Location Table
pub struct CBLCTable<'a> {
    /// Major version of this table.
    ///
    /// 2 for `EBLC`, 3, for `CBLC`
    pub major_version: u16,
    /// Minor version of this table.
    pub minor_version: u16,
    /// Array of "strikes" available for this font.
    pub bitmap_sizes: Vec<BitmapSize<'a>>,
}

/// A description of a "strike" of bitmap data.
pub struct BitmapSize<'a> {
    /// Bitmap information.
    pub inner: BitmapInfo,
    /// Index sub-table records.
    index_sub_table_records: ReadArray<'a, IndexSubTableRecord>,
    /// Index sub-tables, one for each record.
    index_sub_tables: Vec<IndexSubTable<'a>>,
}

#[allow(missing_docs)]
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SbitLineMetrics {
    pub ascender: i8,
    pub descender: i8,
    pub width_max: u8,
    pub caret_slope_numerator: i8,
    pub caret_slope_denominator: i8,
    pub caret_offset: i8,
    pub min_origin_sb: i8,
    pub min_advance_sb: i8,
    pub max_before_bl: i8,
    pub min_after_bl: i8,
    pub pad1: i8,
    pub pad2: i8,
}

/// Subset of BitmapSize that includes common fields.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct BitmapInfo {
    /// Line metrics for text rendered horizontally.
    pub hori: SbitLineMetrics,
    /// Line metrics for text rendered vertically.
    pub vert: SbitLineMetrics,
    /// Lowest glyph index for this size.
    pub start_glyph_index: u16,
    /// Highest glyph index for this size.
    pub end_glyph_index: u16,
    /// Horizontal pixels per em.
    pub ppem_x: u8,
    /// Vertical pixels per em.
    pub ppem_y: u8,
    /// Bit depth.
    ///
    /// In addition to already defined bitDepth values 1, 2, 4, and 8 supported by `EBDT` the value
    /// of 32 is used to identify color bitmaps with 8 bit per channel RGBA channels in `CBDT`.
    pub bit_depth: BitDepth,
    /// Vertical or horizontal.
    pub flags: i8,
}

/// Sub table record of `BitmapSize` describing a range of glyphs and the location of the sub
/// table.
struct IndexSubTableRecord {
    /// First glyph ID of this range.
    pub first_glyph_index: u16,
    /// Last glyph ID of this range (inclusive).
    pub last_glyph_index: u16,
    // Add to indexSubTableArrayOffset to get offset from beginning of EBLC.
    additional_offset_to_index_sub_table: u32,
}

/// An index sub table of a `BitmapSize` describing the image format and location.
///
/// The `IndexSubTable` provides the offset within `CBDT` where the bitmap data for a range of
/// glyphs (described by `IndexSubTableRecord`) can be found, optionally with metrics for the whole
/// range of glyphs as well, depending on the format.
enum IndexSubTable<'a> {
    /// IndexSubTable1: variable-metrics glyphs with 4-byte offsets.
    Format1 {
        /// Format of EBDT image data.
        image_format: ImageFormat,
        /// Offset to image data in EBDT table.
        image_data_offset: u32,
        /// Offsets into `EBDT` for bitmap data.
        ///
        /// The actual offset for a glyph is `image_data_offset` + the value read from this
        /// array.
        offsets: ReadArray<'a, U32Be>,
    },
    /// IndexSubTable2: all glyphs have identical metrics.
    Format2 {
        /// Format of EBDT image data.
        image_format: ImageFormat,
        /// Offset to image data in EBDT table.
        image_data_offset: u32,
        /// The size of the data for each bitmap.
        image_size: u32,
        /// Metrics for all glyphs in this range.
        big_metrics: BigGlyphMetrics,
    },
    /// IndexSubTable3: variable-metrics glyphs with 2-byte offsets.
    Format3 {
        /// Format of EBDT image data.
        image_format: ImageFormat,
        /// Offset to image data in EBDT table.
        image_data_offset: u32,
        /// Offsets into `EBDT` for bitmap data.
        ///
        /// The actual offset for a glyph is `image_data_offset` + the value read from this
        /// array.
        offsets: ReadArray<'a, U16Be>,
    },
    /// IndexSubTable4: variable-metrics glyphs with sparse glyph codes.
    Format4 {
        /// Format of EBDT image data.
        image_format: ImageFormat,
        /// Offset to image data in EBDT table.
        image_data_offset: u32,
        /// Array of ranges.
        glyph_array: ReadArray<'a, GlyphOffsetPair>,
    },
    /// IndexSubTable5: constant-metrics glyphs with sparse glyph codes.
    Format5 {
        /// Format of EBDT image data.
        image_format: ImageFormat,
        /// Offset to image data in EBDT table.
        image_data_offset: u32,
        /// All glyphs have the same data size.
        image_size: u32,
        /// All glyphs have the same metrics.
        big_metrics: BigGlyphMetrics,
        /// One per glyph, sorted by glyph ID.
        glyph_id_array: ReadArray<'a, U16Be>,
    },
}

/// Valid image formats
#[allow(missing_docs)]
#[derive(Copy, Clone)]
pub enum ImageFormat {
    Format1,
    Format2,
    Format5,
    Format6,
    Format7,
    Format8,
    Format9,
    Format17,
    Format18,
    Format19,
}

#[allow(missing_docs)]
#[derive(Debug, Copy, Clone)]
pub struct SmallGlyphMetrics {
    pub height: u8,
    pub width: u8,
    pub bearing_x: i8,
    pub bearing_y: i8,
    pub advance: u8,
}

#[allow(missing_docs)]
#[derive(Debug, Copy, Clone)]
pub struct BigGlyphMetrics {
    pub height: u8,
    pub width: u8,
    pub hori_bearing_x: i8,
    pub hori_bearing_y: i8,
    pub hori_advance: u8,
    pub vert_bearing_x: i8,
    pub vert_bearing_y: i8,
    pub vert_advance: u8,
}

/// The direction of small glyph metrics when present.
enum MetricsDirection {
    Horizontal,
    Vertical,
    Unknown,
}

/// Record indicating the offset in `EBDT` for a specific glyph id.
struct GlyphOffsetPair {
    /// Glyph ID of glyph present.
    pub glyph_id: u16,
    /// Location in EBDT.
    pub offset: u16,
}

/// `CBDT` — Color Bitmap Data Table
pub struct CBDTTable<'a> {
    /// Major version of this table.
    ///
    /// 2 for `EBDT`, 3, for `CBDT`
    pub major_version: u16,
    /// Minor version of this table.
    pub minor_version: u16,
    /// The raw data of the whole `CBDT` table.
    data: ReadScope<'a>,
}

/// Record corresponding to data read from `CBDT`.
pub enum GlyphBitmapData<'a> {
    /// Format 1: small metrics, byte-aligned data.
    Format1 {
        /// Metrics information for the glyph.
        small_metrics: SmallGlyphMetrics,
        /// Byte-aligned bitmap data.
        data: &'a [u8],
    },
    /// Format 2: small metrics, bit-aligned data.
    Format2 {
        /// Metrics information for the glyph.
        small_metrics: SmallGlyphMetrics,
        /// Bit-aligned bitmap data.
        data: &'a [u8],
    },
    // Format3 (obsolete, not in OpenType spec)
    // Format4 (not supported by OpenType, Apple specific)
    /// Format 5: metrics in EBLC, bit-aligned image data only.
    Format5 {
        /// Metrics information for the glyph.
        big_metrics: BigGlyphMetrics,
        /// Bit-aligned bitmap data.
        data: &'a [u8],
    },
    /// Format 6: big metrics, byte-aligned data.
    Format6 {
        /// Metrics information for the glyph.
        big_metrics: BigGlyphMetrics,
        /// Byte-aligned bitmap data.
        data: &'a [u8],
    },
    /// Format7: big metrics, bit-aligned data.
    Format7 {
        /// Metrics information for the glyph.
        big_metrics: BigGlyphMetrics,
        /// Bit-aligned bitmap data.
        data: &'a [u8],
    },
    /// Format 8: small metrics, component data.
    Format8 {
        /// Metrics information for the glyph.
        small_metrics: SmallGlyphMetrics,
        /// Array of EbdtComponent records.
        components: ReadArray<'a, EbdtComponent>,
    },
    /// Format 9: big metrics, component data.
    Format9 {
        /// Metrics information for the glyph.
        big_metrics: BigGlyphMetrics,
        /// Array of EbdtComponent records.
        components: ReadArray<'a, EbdtComponent>,
    },
    // 10-16 are not defined
    /// Format 17: small metrics, PNG image data.
    Format17 {
        /// Metrics information for the glyph.
        small_metrics: SmallGlyphMetrics,
        /// Raw PNG data
        data: &'a [u8],
    },
    /// Format 18: big metrics, PNG image data.
    Format18 {
        /// Metrics information for the glyph.
        big_metrics: BigGlyphMetrics,
        /// Raw PNG data
        data: &'a [u8],
    },
    /// Format 19: metrics in CBLC table, PNG image data.
    Format19 {
        /// Metrics information for the glyph.
        big_metrics: BigGlyphMetrics,
        /// Raw PNG data
        data: &'a [u8],
    },
}

/// The EbdtComponent record is used in glyph bitmap data formats 8 and 9.
pub struct EbdtComponent {
    /// Component glyph ID
    pub glyph_id: u16,
    /// Position of component left
    pub x_offset: i8,
    /// Position of component top
    pub y_offset: i8,
}

/// Result of `find_strike`.
pub struct MatchingStrike<'a, 'b> {
    pub(crate) bitmap_size: &'a BitmapSize<'b>,
    index_subtable_index: usize,
}

struct FontUnitConverter {
    ppem_x: i32,
    ppem_y: i32,
    units_per_em: i32,
}

/// Lookup a glyph in the supplied strike.
///
/// * `glyph_id` is the glyph to lookup.
/// * `matching_strike` the strike to lookup the bitmap in. Acquired via
///   [find_strike](./struct.CBLCTable.html#method.find_strike).
/// * `cbdt` is a reference to the colour bitmap data table.
///
/// The returned `GlyphBitmapData` contains metrics and data for the bitmap, if found.
///
/// **Note:** that some fonts may contain bitmaps with `0x0` dimensions, so be prepared to handle
/// those.
pub fn lookup<'a, 'b>(
    glyph_id: u16,
    matching_strike: &MatchingStrike<'_, '_>,
    cbdt: &CBDTTable<'b>,
) -> Result<Option<GlyphBitmapData<'b>>, ParseError> {
    let index_sub_table_header: &IndexSubTableRecord = &matching_strike
        .bitmap_size
        .index_sub_table_records
        .get_item(matching_strike.index_subtable_index);
    match &matching_strike.bitmap_size.index_sub_tables[matching_strike.index_subtable_index] {
        IndexSubTable::Format1 {
            image_format,
            image_data_offset,
            offsets,
        } => {
            // Should not underflow because find_strike picked a strike that contains this glyph
            let glyph_index = usize::from(glyph_id - index_sub_table_header.first_glyph_index);
            offsets.check_index(glyph_index + 1)?;
            let start = usize::try_from(offsets.get_item(glyph_index))?;
            let end = usize::try_from(offsets.get_item(glyph_index + 1))?;
            let length = end - start;

            if length == 0 {
                // A small number of missing glyphs can be efficiently represented in formats 1 or
                // 3 by having the offset for the missing glyph be followed by the same offset for
                // the next glyph, thus indicating a data size of zero.
                return Ok(None);
            }

            let offset = usize::try_from(*image_data_offset)? + start;
            let mut ctxt = cbdt.data.offset_length(offset, length)?.ctxt();
            let bitmap = ctxt.read_dep::<ImageFormat>((*image_format, None))?;
            Ok(Some(bitmap))
        }
        IndexSubTable::Format2 {
            image_format,
            image_data_offset,
            image_size,
            big_metrics,
        } => {
            let glyph_index = u32::from(glyph_id - index_sub_table_header.first_glyph_index);
            let offset = usize::try_from(image_data_offset + (glyph_index * image_size))?;
            let mut ctxt = cbdt
                .data
                .offset_length(offset, usize::try_from(*image_size)?)?
                .ctxt();
            let bitmap = ctxt.read_dep::<ImageFormat>((*image_format, Some(*big_metrics)))?;
            Ok(Some(bitmap))
        }
        IndexSubTable::Format3 {
            image_format,
            image_data_offset,
            offsets,
        } => {
            // Should not underflow because find_strike picked a strike that contains this glyph
            let glyph_index = usize::from(glyph_id - index_sub_table_header.first_glyph_index);
            offsets.check_index(glyph_index + 1)?;
            let start = usize::from(offsets.get_item(glyph_index));
            let end = usize::from(offsets.get_item(glyph_index + 1));
            let length = end - start;

            if length == 0 {
                // A small number of missing glyphs can be efficiently represented in formats 1 or
                // 3 by having the offset for the missing glyph be followed by the same offset for
                // the next glyph, thus indicating a data size of zero.
                return Ok(None);
            }

            let offset = usize::try_from(*image_data_offset)? + start;
            let mut ctxt = cbdt.data.offset_length(offset, length)?.ctxt();
            let bitmap = ctxt.read_dep::<ImageFormat>((*image_format, None))?;
            Ok(Some(bitmap))
        }
        IndexSubTable::Format4 {
            image_format,
            image_data_offset,
            glyph_array,
        } => {
            // Try to find the desired glyph in the offset pairs
            for (glyph_index, glyph_offset_pair) in glyph_array.iter().enumerate() {
                if glyph_offset_pair.glyph_id == glyph_id {
                    let offset = usize::try_from(*image_data_offset)?
                        + usize::from(glyph_offset_pair.offset);

                    // Get the next pair to determine how big the image data for this glyph is
                    glyph_array.check_index(glyph_index + 1)?;
                    let end = glyph_array.get_item(glyph_index + 1);
                    let length = usize::from(end.offset - glyph_offset_pair.offset);
                    let mut ctxt = cbdt.data.offset_length(offset, length)?.ctxt();
                    let bitmap = ctxt.read_dep::<ImageFormat>((*image_format, None))?;
                    return Ok(Some(bitmap));
                } else if glyph_offset_pair.glyph_id > glyph_id {
                    // Pairs are supposed to be ordered by glyph id so if we're past the one we're
                    // looking for it won't be found.
                    return Ok(None);
                }
            }

            Ok(None)
        }
        IndexSubTable::Format5 {
            image_format,
            image_data_offset,
            image_size,
            big_metrics,
            glyph_id_array,
        } => {
            // Try to find the desired glyph in the list of glyphs covered by this index
            for (glyph_index, this_glyph_id) in glyph_id_array.iter().enumerate() {
                if this_glyph_id == glyph_id {
                    // Found
                    // cast is safe because glyph_id_array num_glyphs is a u32
                    let offset =
                        usize::try_from(image_data_offset + (glyph_index as u32 * image_size))?;
                    let mut ctxt = cbdt
                        .data
                        .offset_length(offset, usize::try_from(*image_size)?)?
                        .ctxt();
                    let bitmap =
                        ctxt.read_dep::<ImageFormat>((*image_format, Some(*big_metrics)))?;
                    return Ok(Some(bitmap));
                } else if this_glyph_id > glyph_id {
                    // Array is meant to be ordered by glyph id so if we're past the one we're
                    // looking for it won't be found.
                    return Ok(None);
                }
            }

            Ok(None)
        }
    }
}

impl<'a> ReadBinaryDep<'a> for ImageFormat {
    type Args = (ImageFormat, Option<BigGlyphMetrics>);
    type HostType = GlyphBitmapData<'a>;

    fn read_dep(
        ctxt: &mut ReadCtxt<'a>,
        (format, metrics): Self::Args,
    ) -> Result<Self::HostType, ParseError> {
        match format {
            ImageFormat::Format1 => {
                let small_metrics = ctxt.read::<SmallGlyphMetrics>()?;
                let data = ctxt.scope().data();

                Ok(GlyphBitmapData::Format1 {
                    small_metrics,
                    data,
                })
            }
            ImageFormat::Format2 => {
                let small_metrics = ctxt.read::<SmallGlyphMetrics>()?;
                let data = ctxt.scope().data();

                Ok(GlyphBitmapData::Format2 {
                    small_metrics,
                    data,
                })
            }
            ImageFormat::Format5 => Ok(GlyphBitmapData::Format5 {
                big_metrics: metrics.ok_or(ParseError::MissingValue)?,
                data: ctxt.scope().data(),
            }),
            ImageFormat::Format6 => {
                let big_metrics = ctxt.read::<BigGlyphMetrics>()?;
                let data = ctxt.scope().data();

                Ok(GlyphBitmapData::Format6 { big_metrics, data })
            }
            ImageFormat::Format7 => {
                let big_metrics = ctxt.read::<BigGlyphMetrics>()?;
                let data = ctxt.scope().data();

                Ok(GlyphBitmapData::Format7 { big_metrics, data })
            }
            ImageFormat::Format8 => {
                let small_metrics = ctxt.read::<SmallGlyphMetrics>()?;
                let _pad = ctxt.read_u8()?;
                let num_components = usize::from(ctxt.read_u16be()?);
                let components = ctxt.read_array::<EbdtComponent>(num_components)?;

                Ok(GlyphBitmapData::Format8 {
                    small_metrics,
                    components,
                })
            }
            ImageFormat::Format9 => {
                let big_metrics = ctxt.read::<BigGlyphMetrics>()?;
                let num_components = usize::from(ctxt.read_u16be()?);
                let components = ctxt.read_array::<EbdtComponent>(num_components)?;

                Ok(GlyphBitmapData::Format9 {
                    big_metrics,
                    components,
                })
            }
            ImageFormat::Format17 => {
                let small_metrics = ctxt.read::<SmallGlyphMetrics>()?;
                let data_len = usize::try_from(ctxt.read_u32be()?)?;
                let data = ctxt.read_slice(data_len)?;

                Ok(GlyphBitmapData::Format17 {
                    small_metrics,
                    data,
                })
            }
            ImageFormat::Format18 => {
                let big_metrics = ctxt.read::<BigGlyphMetrics>()?;
                let data_len = usize::try_from(ctxt.read_u32be()?)?;
                let data = ctxt.read_slice(data_len)?;

                Ok(GlyphBitmapData::Format18 { big_metrics, data })
            }
            ImageFormat::Format19 => {
                let data_len = usize::try_from(ctxt.read_u32be()?)?;
                let data = ctxt.read_slice(data_len)?;

                Ok(GlyphBitmapData::Format19 {
                    big_metrics: metrics.ok_or(ParseError::MissingValue)?,
                    data,
                })
            }
        }
    }
}

impl<'a> CBLCTable<'a> {
    /// Find a strike matching the supplied criteria.
    ///
    /// * `glyph_id` is the glyph to lookup.
    /// * `target_ppem` is the desired size. If an exact match can't be found the nearest one will
    ///    be returned, favouring being oversize vs. undersized.
    /// * `max_bit_depth` is the maximum accepted bit depth of the bitmap to return. If you accept
    ///   all bit depths then use `BitDepth::ThirtyTwo`.
    pub fn find_strike(
        &self,
        glyph_id: u16,
        target_ppem: u8,
        max_bit_depth: BitDepth,
    ) -> Option<MatchingStrike<'_, 'a>> {
        // Find a strike that contains the glyph we want, then find one with an appropriate size
        let candidates = self.bitmap_sizes.iter().filter_map(|bitmap_size| {
            bitmap_size
                .index_sub_table_index(glyph_id)
                .and_then(|index| {
                    if bitmap_size.inner.bit_depth <= max_bit_depth {
                        Some((bitmap_size, index))
                    } else {
                        // Strike has higher bit depth than max_bit_depth
                        None
                    }
                })
        });

        // Pick a candidate that maximises size and bit depth according to `size_ppem` and `max_bit_depth`.
        let size_ppem = i16::from(target_ppem);
        let mut best: Option<(i16, &BitmapSize<'a>, usize)> = None;

        for (bitmap_size, index) in candidates {
            let difference = i16::from(bitmap_size.inner.ppem_x) - size_ppem;
            match best {
                Some((current_best_difference, current_best_bitmap_size, _))
                    if same_size_higher_bit_depth(
                        difference,
                        current_best_difference,
                        bitmap_size.inner.bit_depth,
                        current_best_bitmap_size.inner.bit_depth,
                    ) =>
                {
                    best = Some((difference, bitmap_size, index))
                }
                Some((current_best_difference, _, _))
                    if super::bigger_or_closer_to_zero(difference, current_best_difference) =>
                {
                    best = Some((difference, bitmap_size, index))
                }
                None => best = Some((difference, bitmap_size, index)),
                _ => (),
            }
        }

        best.map(|(_, bitmap_size, index)| MatchingStrike {
            bitmap_size,
            index_subtable_index: index,
        })
    }
}

fn same_size_higher_bit_depth(
    difference: i16,
    current_best_difference: i16,
    candiate_bit_depth: BitDepth,
    current_best_bit_depth: BitDepth,
) -> bool {
    difference == current_best_difference && candiate_bit_depth > current_best_bit_depth
}

impl<'a> ReadBinary<'a> for CBLCTable<'a> {
    type HostType = Self;

    fn read(ctxt: &mut ReadCtxt<'a>) -> Result<Self, ParseError> {
        let table = ctxt.scope();

        let major_version = ctxt.read_u16be()?;
        // version 2 is EBLT, version 3 is CBLC, 3 is backward compatible but defines additional
        // formats and bit depth.
        ctxt.check_version(major_version >= 2 && major_version <= 3)?;
        let minor_version = ctxt.read_u16be()?;
        let num_sizes = ctxt.read_u32be()?;
        let bitmap_sizes = ctxt
            .read_array_dep::<BitmapSize<'_>>(usize::try_from(num_sizes)?, table)?
            .iter_res()
            .collect::<Result<Vec<_>, _>>()?;

        Ok(CBLCTable {
            major_version,
            minor_version,
            bitmap_sizes,
        })
    }
}

impl<'a> ReadBinary<'a> for CBDTTable<'a> {
    type HostType = Self;

    fn read(ctxt: &mut ReadCtxt<'a>) -> Result<Self, ParseError> {
        // The locators in the CBLC table are relative to the start of the CBDT table.
        // So we hold on to a scope at the start of the table for later use.
        let data = ctxt.scope();
        let major_version = ctxt.read_u16be()?;
        // version 2 is EBLT, version 3 is CBLC, 3 is backward compatible but defines additional
        // formats and bit depth.
        ctxt.check_version(major_version >= 2 && major_version <= 3)?;
        let minor_version = ctxt.read_u16be()?;
        Ok(CBDTTable {
            major_version,
            minor_version,
            data,
        })
    }
}

impl<'a> BitmapSize<'a> {
    /// Returns the index of the index sub table for the supplied glyph, if found.
    fn index_sub_table_index(&self, glyph_id: u16) -> Option<usize> {
        // The startGlyphIndex and endGlyphIndex describe the minimum and maximum glyph IDs in the
        // strike, but a strike does not necessarily contain bitmaps for all glyph IDs in this
        // range. The IndexSubTables determine which glyphs are actually present in the CBDT table.
        // https://docs.microsoft.com/en-us/typography/opentype/spec/eblc#sbitlinemetrics
        if (self.inner.start_glyph_index..=self.inner.end_glyph_index).contains(&glyph_id) {
            self.index_sub_table_records
                .iter()
                .position(|record| record.contains_glyph(glyph_id))
        } else {
            None
        }
    }
}

impl<'a, 'b> MatchingStrike<'a, 'b> {
    /// Returns the bit depth of this `MatchingStrike`.
    pub fn bit_depth(&self) -> BitDepth {
        self.bitmap_size.inner.bit_depth
    }
}

impl<'a> ReadBinaryDep<'a> for BitmapSize<'a> {
    type Args = ReadScope<'a>;
    type HostType = Self;

    fn read_dep(ctxt: &mut ReadCtxt<'a>, cblc_scope: Self::Args) -> Result<Self, ParseError> {
        let index_sub_table_array_offset = usize::try_from(ctxt.read_u32be()?)?;
        let _index_tables_size = ctxt.read_u32be()?;
        let number_of_index_sub_tables = ctxt.read_u32be()?;
        let _color_ref = ctxt.read_u32be()?; // Not used; set to 0.
        let hori = ctxt.read::<SbitLineMetrics>()?;
        let vert = ctxt.read::<SbitLineMetrics>()?;
        let start_glyph_index = ctxt.read_u16be()?;
        let end_glyph_index = ctxt.read_u16be()?;
        let ppem_x = ctxt.read_u8()?;
        let ppem_y = ctxt.read_u8()?;
        let bit_depth = BitDepth::try_from(ctxt.read_u8()?)?;
        let flags = ctxt.read_i8()?;

        // Read the index sub tables
        let index_sub_table_records: ReadArray<'_, IndexSubTableRecord> = cblc_scope
            .offset(index_sub_table_array_offset)
            .ctxt()
            .read_array::<IndexSubTableRecord>(usize::try_from(number_of_index_sub_tables)?)?;
        let mut index_sub_tables = Vec::with_capacity(usize::try_from(number_of_index_sub_tables)?);
        for index_sub_table_record in index_sub_table_records.iter() {
            let offset = index_sub_table_array_offset
                .checked_add(usize::try_from(
                    index_sub_table_record.additional_offset_to_index_sub_table,
                )?)
                .ok_or(ParseError::BadOffset)?;
            // Read the index sub table
            let index_sub_table = cblc_scope
                .offset(offset)
                .ctxt()
                .read_dep::<IndexSubTable<'_>>((
                    index_sub_table_record.first_glyph_index,
                    index_sub_table_record.last_glyph_index,
                ))?;
            index_sub_tables.push(index_sub_table);
        }

        Ok(BitmapSize {
            inner: BitmapInfo {
                hori,
                vert,
                start_glyph_index,
                end_glyph_index,
                ppem_x,
                ppem_y,
                bit_depth,
                flags,
            },
            index_sub_table_records,
            index_sub_tables,
        })
    }
}

impl<'a> ReadFixedSizeDep<'a> for BitmapSize<'a> {
    fn size(_: Self::Args) -> usize {
        // Offset32         indexSubTableArrayOffset
        // uint32           indexTablesSize
        // uint32           numberofIndexSubTables
        // uint32           colorRef
        (4 * size::U32)
        // SbitLineMetrics  hori
        // SbitLineMetrics  vert
        + (2 * SbitLineMetrics::size(()))
        // uint16           startGlyphIndex
        // uint16           endGlyphIndex
        + (2 * size::U16)
        // uint8            ppemX
        // uint8            ppemY
        // uint8            bitDepth
        // int8             flags
        + 4
    }
}

impl<'a> ReadBinary<'a> for SbitLineMetrics {
    type HostType = Self;

    fn read(ctxt: &mut ReadCtxt<'a>) -> Result<Self, ParseError> {
        let ascender = ctxt.read_i8()?;
        let descender = ctxt.read_i8()?;
        let width_max = ctxt.read_u8()?;
        let caret_slope_numerator = ctxt.read_i8()?;
        let caret_slope_denominator = ctxt.read_i8()?;
        let caret_offset = ctxt.read_i8()?;
        let min_origin_sb = ctxt.read_i8()?;
        let min_advance_sb = ctxt.read_i8()?;
        let max_before_bl = ctxt.read_i8()?;
        let min_after_bl = ctxt.read_i8()?;
        let pad1 = ctxt.read_i8()?;
        let pad2 = ctxt.read_i8()?;

        Ok(SbitLineMetrics {
            ascender,
            descender,
            width_max,
            caret_slope_numerator,
            caret_slope_denominator,
            caret_offset,
            min_origin_sb,
            min_advance_sb,
            max_before_bl,
            min_after_bl,
            pad1,
            pad2,
        })
    }
}

impl<'a> ReadFixedSizeDep<'a> for SbitLineMetrics {
    fn size(_scope: Self::Args) -> usize {
        // 12 fields, all 1 byte
        12
    }
}

impl TryFrom<u8> for BitDepth {
    type Error = ParseError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(BitDepth::One),
            2 => Ok(BitDepth::Two),
            4 => Ok(BitDepth::Four),
            8 => Ok(BitDepth::Eight),
            32 => Ok(BitDepth::ThirtyTwo),
            _ => Err(ParseError::BadValue),
        }
    }
}

impl<'a> IndexSubTableRecord {
    fn contains_glyph(&self, glyph_id: u16) -> bool {
        (self.first_glyph_index..=self.last_glyph_index).contains(&glyph_id)
    }
}

impl<'a> ReadFrom<'a> for IndexSubTableRecord {
    type ReadType = (U16Be, U16Be, U32Be);

    fn from(
        (first_glyph_index, last_glyph_index, additional_offset_to_index_sub_table): (
            u16,
            u16,
            u32,
        ),
    ) -> Self {
        IndexSubTableRecord {
            first_glyph_index,
            last_glyph_index,
            additional_offset_to_index_sub_table,
        }
    }
}

impl<'a> ReadBinaryDep<'a> for IndexSubTable<'a> {
    type Args = (u16, u16);
    type HostType = Self;

    fn read_dep(
        ctxt: &mut ReadCtxt<'a>,
        (first_glyph_index, last_glyph_index): (u16, u16),
    ) -> Result<Self, ParseError> {
        let index_format = ctxt.read_u16be()?;
        let image_format = ImageFormat::try_from(ctxt.read_u16be()?)?;
        let image_data_offset = ctxt.read_u32be()?;

        match index_format {
            1 => {
                // +1 for last_glyph_index being inclusive,
                // +1 for there being an extra record at the end
                let offsets = ctxt.read_array::<U32Be>(usize::from(
                    last_glyph_index - first_glyph_index + 1 + 1,
                ))?;
                Ok(IndexSubTable::Format1 {
                    image_format,
                    image_data_offset,
                    offsets,
                })
            }
            2 => {
                let image_size = ctxt.read_u32be()?;
                let big_metrics = ctxt.read::<BigGlyphMetrics>()?;
                Ok(IndexSubTable::Format2 {
                    image_format,
                    image_data_offset,
                    image_size,
                    big_metrics,
                })
            }
            3 => {
                // +1 for last_glyph_index being inclusive,
                // +1 for there being an extra record at the end
                let offsets = ctxt.read_array::<U16Be>(usize::from(
                    last_glyph_index - first_glyph_index + 1 + 1,
                ))?;
                Ok(IndexSubTable::Format3 {
                    image_format,
                    image_data_offset,
                    offsets,
                })
            }
            4 => {
                let num_glyphs = ctxt.read_u32be()?;
                let glyph_array =
                    ctxt.read_array::<GlyphOffsetPair>(usize::try_from(num_glyphs + 1)?)?;
                Ok(IndexSubTable::Format4 {
                    image_format,
                    image_data_offset,
                    glyph_array,
                })
            }
            5 => {
                let image_size = ctxt.read_u32be()?;
                let big_metrics = ctxt.read::<BigGlyphMetrics>()?;
                let num_glyphs = ctxt.read_u32be()?;
                let glyph_id_array = ctxt.read_array::<U16Be>(usize::try_from(num_glyphs)?)?;
                Ok(IndexSubTable::Format5 {
                    image_format,
                    image_data_offset,
                    image_size,
                    big_metrics,
                    glyph_id_array,
                })
            }
            _ => Err(ParseError::BadValue),
        }
    }
}

impl<'a> ReadFrom<'a> for SmallGlyphMetrics {
    type ReadType = ((U8, U8), (I8, I8, U8));

    fn from(((height, width), (bearing_x, bearing_y, advance)): ((u8, u8), (i8, i8, u8))) -> Self {
        SmallGlyphMetrics {
            height,
            width,
            bearing_x,
            bearing_y,
            advance,
        }
    }
}

impl<'a> ReadBinary<'a> for BigGlyphMetrics {
    type HostType = Self;

    fn read(ctxt: &mut ReadCtxt<'a>) -> Result<Self, ParseError> {
        let height = ctxt.read_u8()?;
        let width = ctxt.read_u8()?;
        let hori_bearing_x = ctxt.read_i8()?;
        let hori_bearing_y = ctxt.read_i8()?;
        let hori_advance = ctxt.read_u8()?;
        let vert_bearing_x = ctxt.read_i8()?;
        let vert_bearing_y = ctxt.read_i8()?;
        let vert_advance = ctxt.read_u8()?;

        Ok(BigGlyphMetrics {
            height,
            width,
            hori_bearing_x,
            hori_bearing_y,
            hori_advance,
            vert_bearing_x,
            vert_bearing_y,
            vert_advance,
        })
    }
}

impl<'a> ReadFixedSizeDep<'a> for BigGlyphMetrics {
    fn size(_scope: Self::Args) -> usize {
        // 8 fields, all 1 byte
        8
    }
}

impl<'a> ReadFrom<'a> for GlyphOffsetPair {
    type ReadType = (U16Be, U16Be);

    fn from((glyph_id, offset): (u16, u16)) -> Self {
        GlyphOffsetPair { glyph_id, offset }
    }
}

impl<'a> ReadFrom<'a> for EbdtComponent {
    type ReadType = (U16Be, I8, I8);

    fn from((glyph_id, x_offset, y_offset): (u16, i8, i8)) -> Self {
        EbdtComponent {
            glyph_id,
            x_offset,
            y_offset,
        }
    }
}

impl TryFrom<u16> for ImageFormat {
    type Error = ParseError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(ImageFormat::Format1),
            2 => Ok(ImageFormat::Format2),
            5 => Ok(ImageFormat::Format5),
            6 => Ok(ImageFormat::Format6),
            7 => Ok(ImageFormat::Format7),
            8 => Ok(ImageFormat::Format8),
            9 => Ok(ImageFormat::Format9),
            17 => Ok(ImageFormat::Format17),
            18 => Ok(ImageFormat::Format18),
            19 => Ok(ImageFormat::Format19),
            _ => Err(ParseError::BadValue),
        }
    }
}

impl<'a> TryFrom<(&BitmapInfo, GlyphBitmapData<'a>, u16)> for BitmapGlyph {
    type Error = ParseError;

    fn try_from(
        (info, glyph, units_per_em): (&BitmapInfo, GlyphBitmapData<'a>, u16),
    ) -> Result<Self, Self::Error> {
        let res = match glyph {
            // Format 1: small metrics, byte-aligned data.
            GlyphBitmapData::Format1 {
                small_metrics,
                data,
            } => {
                let data = bgra_to_rgba(info.bit_depth, data.to_vec())?;
                let metrics = EmbeddedMetrics::try_from((info, &small_metrics, units_per_em))?;
                BitmapGlyph {
                    bitmap: Bitmap::Embedded(EmbeddedBitmap {
                        format: info.bit_depth,
                        width: small_metrics.width,
                        height: small_metrics.height,
                        data: Box::from(data),
                    }),
                    metrics: Metrics::Embedded(metrics),
                    ppem_x: u16::from(info.ppem_x),
                    ppem_y: u16::from(info.ppem_y),
                }
            }
            // Format 2: small metrics, bit-aligned data.
            GlyphBitmapData::Format2 {
                small_metrics,
                data,
            } => {
                let metrics = EmbeddedMetrics::try_from((info, &small_metrics, units_per_em))?;
                let unpacked = unpack_bit_aligned_data(
                    info.bit_depth,
                    small_metrics.width,
                    small_metrics.height,
                    data,
                )
                .map_err(parse_error_from_bitreader_error)
                .and_then(|data| bgra_to_rgba(info.bit_depth, data))?;
                BitmapGlyph {
                    bitmap: Bitmap::Embedded(EmbeddedBitmap {
                        format: info.bit_depth,
                        width: small_metrics.width,
                        height: small_metrics.height,
                        data: unpacked.into(),
                    }),
                    metrics: Metrics::Embedded(metrics),
                    ppem_x: u16::from(info.ppem_x),
                    ppem_y: u16::from(info.ppem_y),
                }
            }
            // Format 5: metrics in EBLC, bit-aligned image data only.
            GlyphBitmapData::Format5 { big_metrics, data } => {
                let metrics = EmbeddedMetrics::try_from((info, &big_metrics, units_per_em))?;
                let unpacked = unpack_bit_aligned_data(
                    info.bit_depth,
                    big_metrics.width,
                    big_metrics.height,
                    data,
                )
                .map_err(parse_error_from_bitreader_error)
                .and_then(|data| bgra_to_rgba(info.bit_depth, data))?;
                BitmapGlyph {
                    bitmap: Bitmap::Embedded(EmbeddedBitmap {
                        format: info.bit_depth,
                        width: big_metrics.width,
                        height: big_metrics.height,
                        data: unpacked.into(),
                    }),
                    metrics: Metrics::Embedded(metrics),
                    ppem_x: u16::from(info.ppem_x),
                    ppem_y: u16::from(info.ppem_y),
                }
            }
            // Format 6: big metrics, byte-aligned data.
            GlyphBitmapData::Format6 { big_metrics, data } => {
                let data = bgra_to_rgba(info.bit_depth, data.to_vec())?;
                let metrics = EmbeddedMetrics::try_from((info, &big_metrics, units_per_em))?;
                BitmapGlyph {
                    bitmap: Bitmap::Embedded(EmbeddedBitmap {
                        format: info.bit_depth,
                        width: big_metrics.width,
                        height: big_metrics.height,
                        data: Box::from(data),
                    }),
                    metrics: Metrics::Embedded(metrics),
                    ppem_x: u16::from(info.ppem_x),
                    ppem_y: u16::from(info.ppem_y),
                }
            }
            // Format7: big metrics, bit-aligned data.
            GlyphBitmapData::Format7 { big_metrics, data } => {
                let metrics = EmbeddedMetrics::try_from((info, &big_metrics, units_per_em))?;
                let unpacked = unpack_bit_aligned_data(
                    info.bit_depth,
                    big_metrics.width,
                    big_metrics.height,
                    data,
                )
                .map_err(parse_error_from_bitreader_error)
                .and_then(|data| bgra_to_rgba(info.bit_depth, data))?;
                BitmapGlyph {
                    bitmap: Bitmap::Embedded(EmbeddedBitmap {
                        format: info.bit_depth,
                        width: big_metrics.width,
                        height: big_metrics.height,
                        data: unpacked.into(),
                    }),
                    metrics: Metrics::Embedded(metrics),
                    ppem_x: u16::from(info.ppem_x),
                    ppem_y: u16::from(info.ppem_y),
                }
            }
            // Format 8: small metrics, component data.
            GlyphBitmapData::Format8 { .. } => return Err(ParseError::NotImplemented),
            // Format 9: big metrics, component data.
            GlyphBitmapData::Format9 { .. } => return Err(ParseError::NotImplemented),
            // Format 17: small metrics, PNG image data.
            GlyphBitmapData::Format17 {
                small_metrics,
                data,
            } => {
                let metrics = EmbeddedMetrics::try_from((info, &small_metrics, units_per_em))?;
                let bitmap = EncapsulatedBitmap {
                    format: EncapsulatedFormat::Png,
                    data: Box::from(data),
                };
                BitmapGlyph {
                    bitmap: Bitmap::Encapsulated(bitmap),
                    metrics: Metrics::Embedded(metrics),
                    ppem_x: u16::from(info.ppem_x),
                    ppem_y: u16::from(info.ppem_y),
                }
            }
            // Format 18: big metrics, PNG image data.
            // Format 19: metrics in CBLC table, PNG image data.
            GlyphBitmapData::Format18 { big_metrics, data }
            | GlyphBitmapData::Format19 { big_metrics, data } => {
                let metrics = EmbeddedMetrics::try_from((info, &big_metrics, units_per_em))?;
                let bitmap = EncapsulatedBitmap {
                    format: EncapsulatedFormat::Png,
                    data: Box::from(data),
                };
                BitmapGlyph {
                    bitmap: Bitmap::Encapsulated(bitmap),
                    metrics: Metrics::Embedded(metrics),
                    ppem_x: u16::from(info.ppem_x),
                    ppem_y: u16::from(info.ppem_y),
                }
            }
        };

        Ok(res)
    }
}

impl<'a> GlyphBitmapData<'a> {
    /// The width of the bitmap.
    pub fn width(&self) -> u8 {
        match self {
            GlyphBitmapData::Format1 {
                small_metrics: SmallGlyphMetrics { width, .. },
                ..
            } => *width,
            GlyphBitmapData::Format2 {
                small_metrics: SmallGlyphMetrics { width, .. },
                ..
            } => *width,
            GlyphBitmapData::Format5 {
                big_metrics: BigGlyphMetrics { width, .. },
                ..
            } => *width,
            GlyphBitmapData::Format6 {
                big_metrics: BigGlyphMetrics { width, .. },
                ..
            } => *width,
            GlyphBitmapData::Format7 {
                big_metrics: BigGlyphMetrics { width, .. },
                ..
            } => *width,
            GlyphBitmapData::Format8 {
                small_metrics: SmallGlyphMetrics { width, .. },
                ..
            } => *width,
            GlyphBitmapData::Format9 {
                big_metrics: BigGlyphMetrics { width, .. },
                ..
            } => *width,
            GlyphBitmapData::Format17 {
                small_metrics: SmallGlyphMetrics { width, .. },
                ..
            } => *width,
            GlyphBitmapData::Format18 {
                big_metrics: BigGlyphMetrics { width, .. },
                ..
            } => *width,
            GlyphBitmapData::Format19 {
                big_metrics: BigGlyphMetrics { width, .. },
                ..
            } => *width,
        }
    }

    /// The height of the bitmap.
    pub fn height(&self) -> u8 {
        match self {
            GlyphBitmapData::Format1 {
                small_metrics: SmallGlyphMetrics { height, .. },
                ..
            } => *height,
            GlyphBitmapData::Format2 {
                small_metrics: SmallGlyphMetrics { height, .. },
                ..
            } => *height,
            GlyphBitmapData::Format5 {
                big_metrics: BigGlyphMetrics { height, .. },
                ..
            } => *height,
            GlyphBitmapData::Format6 {
                big_metrics: BigGlyphMetrics { height, .. },
                ..
            } => *height,
            GlyphBitmapData::Format7 {
                big_metrics: BigGlyphMetrics { height, .. },
                ..
            } => *height,
            GlyphBitmapData::Format8 {
                small_metrics: SmallGlyphMetrics { height, .. },
                ..
            } => *height,
            GlyphBitmapData::Format9 {
                big_metrics: BigGlyphMetrics { height, .. },
                ..
            } => *height,
            GlyphBitmapData::Format17 {
                small_metrics: SmallGlyphMetrics { height, .. },
                ..
            } => *height,
            GlyphBitmapData::Format18 {
                big_metrics: BigGlyphMetrics { height, .. },
                ..
            } => *height,
            GlyphBitmapData::Format19 {
                big_metrics: BigGlyphMetrics { height, .. },
                ..
            } => *height,
        }
    }
}

impl<'a> fmt::Debug for GlyphBitmapData<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GlyphBitmapData::Format1 {
                small_metrics,
                data,
            } => f
                .debug_struct("GlyphBitmapData::Format1")
                .field("small_metrics", &small_metrics)
                .field("data", &format_args!("[{} bytes]", data.len()))
                .finish(),
            GlyphBitmapData::Format2 {
                small_metrics,
                data,
            } => f
                .debug_struct("GlyphBitmapData::Format2")
                .field("small_metrics", &small_metrics)
                .field("data", &format_args!("[{} bytes]", data.len()))
                .finish(),
            GlyphBitmapData::Format5 { big_metrics, data } => f
                .debug_struct("GlyphBitmapData::Format5")
                .field("big_metrics", &big_metrics)
                .field("data", &format_args!("[{} bytes]", data.len()))
                .finish(),
            GlyphBitmapData::Format6 { big_metrics, data } => f
                .debug_struct("GlyphBitmapData::Format6")
                .field("big_metrics", &big_metrics)
                .field("data", &format_args!("[{} bytes]", data.len()))
                .finish(),
            GlyphBitmapData::Format7 { big_metrics, data } => f
                .debug_struct("GlyphBitmapData::Format7")
                .field("big_metrics", &big_metrics)
                .field("data", &format_args!("[{} bytes]", data.len()))
                .finish(),
            GlyphBitmapData::Format8 { small_metrics, .. } => f
                .debug_struct("GlyphBitmapData::Format8")
                .field("small_metrics", &small_metrics)
                .finish(),
            GlyphBitmapData::Format9 { big_metrics, .. } => f
                .debug_struct("GlyphBitmapData::Format9")
                .field("big_metrics", &big_metrics)
                .finish(),
            GlyphBitmapData::Format17 {
                small_metrics,
                data,
            } => f
                .debug_struct("GlyphBitmapData::Format17")
                .field("small_metrics", &small_metrics)
                .field("data", &format_args!("[{} bytes]", data.len()))
                .finish(),
            GlyphBitmapData::Format18 { big_metrics, data } => f
                .debug_struct("GlyphBitmapData::Format18")
                .field("big_metrics", &big_metrics)
                .field("data", &format_args!("[{} bytes]", data.len()))
                .finish(),
            GlyphBitmapData::Format19 { big_metrics, data } => f
                .debug_struct("GlyphBitmapData::Format19")
                .field("big_metrics", &big_metrics)
                .field("data", &format_args!("[{} bytes]", data.len()))
                .finish(),
        }
    }
}

impl EmbeddedMetrics {
    fn new(hori: Option<BitmapMetrics>, vert: Option<BitmapMetrics>) -> Result<Self, ParseError> {
        if hori.is_none() && vert.is_none() {
            return Err(ParseError::MissingValue);
        }

        Ok(EmbeddedMetrics { hori, vert })
    }

    /// Metrics for horizontal layout.
    pub fn hori(&self) -> Option<&BitmapMetrics> {
        self.hori.as_ref()
    }

    /// Metrics for vertical layout.
    pub fn vert(&self) -> Option<&BitmapMetrics> {
        self.vert.as_ref()
    }
}

impl TryFrom<(&BitmapInfo, &SmallGlyphMetrics, u16)> for EmbeddedMetrics {
    type Error = ParseError;

    fn try_from(
        (info, small_metrics, units_per_em): (&BitmapInfo, &SmallGlyphMetrics, u16),
    ) -> Result<Self, Self::Error> {
        let pixels = FontUnitConverter::new(info.ppem_x, info.ppem_y, units_per_em);
        match info.small_glyph_metrics_direction() {
            MetricsDirection::Horizontal | MetricsDirection::Unknown => EmbeddedMetrics::new(
                Some(BitmapMetrics {
                    origin_offset: OriginOffset {
                        x: pixels.to_font_units_x(small_metrics.bearing_x).try_into()?,
                        y: pixels.to_font_units_x(small_metrics.bearing_y).try_into()?, // TODO: Convert
                    },
                    advance: pixels.to_font_units_x(small_metrics.advance).try_into()?,
                    ascender: pixels.to_font_units_x(info.hori.ascender).try_into()?,
                    descender: pixels.to_font_units_x(info.hori.descender).try_into()?,
                }),
                None,
            ),
            MetricsDirection::Vertical => EmbeddedMetrics::new(
                None,
                Some(BitmapMetrics {
                    origin_offset: OriginOffset {
                        x: pixels.to_font_units_y(small_metrics.bearing_x).try_into()?,
                        y: pixels.to_font_units_y(small_metrics.bearing_y).try_into()?, // TODO: Convert
                    },
                    advance: pixels.to_font_units_y(small_metrics.advance).try_into()?,
                    ascender: pixels.to_font_units_y(info.vert.ascender).try_into()?,
                    descender: pixels.to_font_units_y(info.vert.descender).try_into()?,
                }),
            ),
        }
    }
}

impl TryFrom<(&BitmapInfo, &BigGlyphMetrics, u16)> for EmbeddedMetrics {
    type Error = ParseError;

    fn try_from(
        (info, big_metrics, units_per_em): (&BitmapInfo, &BigGlyphMetrics, u16),
    ) -> Result<Self, Self::Error> {
        let pixels = FontUnitConverter::new(info.ppem_x, info.ppem_y, units_per_em);
        Ok(EmbeddedMetrics {
            hori: Some(BitmapMetrics {
                origin_offset: OriginOffset {
                    x: pixels
                        .to_font_units_x(big_metrics.hori_bearing_x)
                        .try_into()?,
                    y: pixels
                        .to_font_units_x(big_metrics.hori_bearing_y)
                        .try_into()?, // FIXME: need to convert from top to bottom of bitmap
                },
                advance: pixels
                    .to_font_units_x(big_metrics.hori_advance)
                    .try_into()?,
                ascender: i16::from(info.hori.ascender),
                descender: i16::from(info.hori.descender),
            }),
            vert: Some(BitmapMetrics {
                origin_offset: OriginOffset {
                    x: pixels
                        .to_font_units_y(big_metrics.vert_bearing_x)
                        .try_into()?,
                    y: pixels
                        .to_font_units_y(big_metrics.vert_bearing_y)
                        .try_into()?, // FIXME: need to convert from top to bottom of bitmap
                },
                advance: pixels
                    .to_font_units_y(big_metrics.vert_advance)
                    .try_into()?,
                ascender: i16::from(info.vert.ascender),
                descender: i16::from(info.vert.descender),
            }),
        })
    }
}

impl BitmapInfo {
    fn small_glyph_metrics_direction(&self) -> MetricsDirection {
        if self.flags & HORIZONTAL_METRICS == HORIZONTAL_METRICS {
            MetricsDirection::Horizontal
        } else if self.flags & VERTICAL_METRICS == VERTICAL_METRICS {
            MetricsDirection::Vertical
        } else {
            MetricsDirection::Unknown
        }
    }
}

impl FontUnitConverter {
    fn new(ppem_x: u8, ppem_y: u8, units_per_em: u16) -> Self {
        FontUnitConverter {
            ppem_x: i32::from(ppem_x),
            ppem_y: i32::from(ppem_y),
            units_per_em: i32::from(units_per_em),
        }
    }

    /// Convert a pixel value to a value in font design units using ppem_x.
    ///
    /// Pixel values are read from bitmap tables like `EBLC` and `CBLC`. The `head`
    /// table contains a units per em value that can be used to convert from pixels
    /// to font design units.
    fn to_font_units_x<V>(&self, value: V) -> i32
    where
        V: Into<i32>,
    {
        value.into() * self.units_per_em / self.ppem_x
    }

    /// Convert a pixel value to a value in font design units using ppem_y.
    ///
    /// Pixel values are read from bitmap tables like `EBLC` and `CBLC`. The `head`
    /// table contains a units per em value that can be used to convert from pixels
    /// to font design units.
    fn to_font_units_y<V>(&self, value: V) -> i32
    where
        V: Into<i32>,
    {
        value.into() * self.units_per_em / self.ppem_y
    }
}

fn unpack_bit_aligned_data(
    bit_depth: BitDepth,
    width: u8,
    height: u8,
    data: &[u8],
) -> Result<Vec<u8>, BitReaderError> {
    let bits_per_row = bit_depth as usize * usize::from(width);
    let whole_bytes_per_row = bits_per_row >> 3;
    let remaining_bits = (bits_per_row & 7) as u8;
    let bytes_per_row = whole_bytes_per_row + if remaining_bits != 0 { 1 } else { 0 };

    let mut offset = 0;
    let mut image_data = vec![0u8; usize::from(height) * bytes_per_row];
    let mut reader = BitReader::new(data);
    for _ in 0..height {
        // Read whole bytes, then the remainder
        for byte in image_data[offset..(offset + whole_bytes_per_row)].iter_mut() {
            *byte = reader.read_u8(8)?;
        }
        offset += whole_bytes_per_row;
        if remaining_bits != 0 {
            let byte = reader.read_u8(remaining_bits)?;
            image_data[offset] = byte << (8 - remaining_bits);
            offset += 1;
        }
    }

    Ok(image_data)
}

fn parse_error_from_bitreader_error(err: BitReaderError) -> ParseError {
    match err {
        BitReaderError::NotEnoughData { .. } => ParseError::BadEof,
        BitReaderError::TooManyBitsForType { .. } => {
            // This should only happen as a result of programmer error as we only call bitreader
            // with values <= 8.
            unreachable!("{}", err)
        }
    }
}

fn bgra_to_rgba(bit_depth: BitDepth, mut data: Vec<u8>) -> Result<Vec<u8>, ParseError> {
    match bit_depth {
        BitDepth::One | BitDepth::Two | BitDepth::Four | BitDepth::Eight => Ok(data),
        BitDepth::ThirtyTwo => {
            if data.len() % 4 != 0 {
                return Err(ParseError::BadEof);
            }
            data.chunks_exact_mut(4).for_each(|chunk| chunk.swap(0, 2));
            Ok(data)
        }
    }
}

#[cfg(test)]
mod tests {
    use itertools::Itertools;
    use std::borrow::Borrow;
    use std::path::Path;

    use super::*;
    use crate::fontfile::FontFile;
    use crate::tables::FontTableProvider;
    use crate::tag;
    use crate::tests::read_fixture;

    #[test]
    fn test_parse_cblc() {
        let cblc_data = read_fixture(Path::new("tests/fonts/opentype/CBLC.bin"));
        let cblc = ReadScope::new(&cblc_data).read::<CBLCTable<'_>>().unwrap();

        let strikes = &cblc.bitmap_sizes;
        assert_eq!(strikes.len(), 1);
        assert_eq!(strikes[0].index_sub_tables.len(), 3);
        let ranges = strikes[0]
            .index_sub_table_records
            .iter()
            .map(|rec| rec.first_glyph_index..=rec.last_glyph_index)
            .collect_vec();
        assert_eq!(ranges, &[4..=17, 19..=1316, 1354..=3112]);
    }

    #[test]
    fn test_parse_eblc() {
        let buffer = read_fixture(Path::new("tests/fonts/opentype/TerminusTTF-4.47.0.ttf"));
        let scope = ReadScope::new(&buffer);
        let font_file = scope
            .read::<FontFile<'_>>()
            .expect("unable to parse font file");
        let table_provider = font_file
            .table_provider(0)
            .expect("unable to create font provider");
        let table = table_provider
            .table_data(tag::EBLC)
            .expect("no EBLC table")
            .expect("no EBLC table");
        let scope = ReadScope::new(table.borrow());
        let eblc = scope.read::<CBLCTable<'_>>().unwrap();

        let strikes = &eblc.bitmap_sizes;
        assert_eq!(strikes.len(), 9);
    }

    #[test]
    fn test_lookup_eblc() {
        let buffer = read_fixture(Path::new("tests/fonts/opentype/TerminusTTF-4.47.0.ttf"));
        let scope = ReadScope::new(&buffer);
        let font_file = scope
            .read::<FontFile<'_>>()
            .expect("unable to parse font file");
        let table_provider = font_file
            .table_provider(0)
            .expect("unable to create font provider");
        let table = table_provider
            .table_data(tag::EBLC)
            .expect("no EBLC table")
            .expect("no EBLC table");
        let scope = ReadScope::new(table.borrow());
        let eblc = scope.read::<CBLCTable<'_>>().unwrap();
        let table = table_provider
            .table_data(tag::EBDT)
            .expect("no EBDT table")
            .expect("no EBDT table");
        let scope = ReadScope::new(table.borrow());
        let ebdt = scope.read::<CBDTTable<'_>>().unwrap();

        // Font has strikes in 12 14 16 18 20 22 24 28 32 ppem
        // Glyph 10 is ampersand
        let strike = eblc
            .find_strike(10, 30, BitDepth::ThirtyTwo)
            .expect("no matching strike");
        let res = lookup(10, &strike, &ebdt).expect("error looking up glyph");
        match res {
            Some(GlyphBitmapData::Format5 { data, .. }) => assert_eq!(data.len(), 64),
            _ => panic!("expected GlyphBitmapData::Format5 got something else"),
        }
    }

    #[test]
    fn test_lookup_cblc() {
        // Test tables are from Noto Color Emoji
        let cblc_data = read_fixture(Path::new("tests/fonts/opentype/CBLC.bin"));
        let cblc = ReadScope::new(&cblc_data).read::<CBLCTable<'_>>().unwrap();
        let cbdt_data = read_fixture(Path::new("tests/fonts/opentype/CBDT.bin"));
        let cbdt = ReadScope::new(&cbdt_data).read::<CBDTTable<'_>>().unwrap();

        // Glyph 1077 is Nerd Face U+1F913
        let strike = cblc
            .find_strike(1077, 30, BitDepth::ThirtyTwo)
            .expect("no matching strike");
        let res = lookup(1077, &strike, &cbdt).expect("error looking up glyph");
        match res {
            Some(GlyphBitmapData::Format17 {
                data,
                small_metrics: SmallGlyphMetrics { width, height, .. },
            }) => {
                assert_eq!((width, height), (136, 128));
                assert_eq!(&data[1..4], b"PNG");
            }
            _ => panic!("expected PNG data got something else"),
        }

        // Repeat the lookup with a lower max bit depth, should now fail to find suitable strike
        assert!(cblc.find_strike(1077, 30, BitDepth::Four).is_none());
    }

    #[test]
    fn test_unpack_bit_aligned_data() {
        let data = &[0xD3, 0xAA, 0x70];
        let expected = &[0xD3, 0x80, 0xA9, 0xC0];
        let actual = unpack_bit_aligned_data(BitDepth::Two, 5, 2, data).unwrap();
        assert_eq!(&actual, expected);
    }

    #[test]
    fn test_bgra_to_rgba_no_change() {
        let original = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let actual = bgra_to_rgba(BitDepth::One, original.clone()).unwrap();
        assert_eq!(actual, original);
    }

    #[test]
    fn test_bgra_to_rgba_reorder() {
        let data = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let expected = &[3, 2, 1, 4, 7, 6, 5, 8];
        let actual = bgra_to_rgba(BitDepth::ThirtyTwo, data).unwrap();
        assert_eq!(&actual, expected);
    }

    #[test]
    fn test_bgra_to_rgba_too_short() {
        let data = vec![1, 2, 3, 4, 5, 6, 7];
        let res = bgra_to_rgba(BitDepth::ThirtyTwo, data);
        assert_eq!(res, Err(ParseError::BadEof));
    }
}
