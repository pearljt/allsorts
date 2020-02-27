//! CFF font handling.
//!
//! Refer to [Technical Note #5176](http://wwwimages.adobe.com/content/dam/Adobe/en/devnet/font/pdfs/5176.CFF.pdf)
//! for more information.

use std::convert::{TryFrom, TryInto};
use std::iter;
use std::marker::PhantomData;
use std::mem;

use byteorder::{BigEndian, ByteOrder};
use itertools::Itertools;
use lazy_static::lazy_static;
use num_traits as num;

use crate::binary::read::{
    CheckIndex, ReadArray, ReadArrayCow, ReadBinary, ReadBinaryDep, ReadCtxt, ReadFrom, ReadScope,
    ReadUnchecked,
};
use crate::binary::write::{WriteBinary, WriteBinaryDep, WriteBuffer, WriteContext, WriteCounter};
use crate::binary::{I16Be, I32Be, U16Be, U24Be, U32Be, U8};
use crate::error::{ParseError, WriteError};

// CFF Spec: An operator may be preceded by up to a maximum of 48 operands.
const MAX_OPERANDS: usize = 48;

const OPERAND_ZERO: [Operand; 1] = [Operand::Integer(0)];
const OFFSET_ZERO: [Operand; 1] = [Operand::Offset(0)];
const DEFAULT_UNDERLINE_POSITION: [Operand; 1] = [Operand::Integer(-100)];
const DEFAULT_UNDERLINE_THICKNESS: [Operand; 1] = [Operand::Integer(50)];
const DEFAULT_CHARSTRING_TYPE: [Operand; 1] = [Operand::Integer(2)];
lazy_static! {
    static ref DEFAULT_FONT_MATRIX: [Operand; 6] = {
        let real_0_001 = Operand::Real(Real(vec![0x0a, 0x00, 0x1f])); // 0.001
        [
            real_0_001.clone(),
            Operand::Integer(0),
            Operand::Integer(0),
            real_0_001,
            Operand::Integer(0),
            Operand::Integer(0),
        ]
    };
}
const DEFAULT_BBOX: [Operand; 4] = [
    Operand::Integer(0),
    Operand::Integer(0),
    Operand::Integer(0),
    Operand::Integer(0),
];
const DEFAULT_CID_COUNT: [Operand; 1] = [Operand::Integer(8720)];
const DEFAULT_BLUE_SHIFT: [Operand; 1] = [Operand::Integer(7)];
const DEFAULT_BLUE_FUZZ: [Operand; 1] = [Operand::Integer(1)];
lazy_static! {
    static ref DEFAULT_BLUE_SCALE: [Operand; 1] =
        [Operand::Real(Real(vec![0x0a, 0x03, 0x96, 0x25, 0xff]))]; // 0.039625
    static ref DEFAULT_EXPANSION_FACTOR: [Operand; 1] =
        [Operand::Real(Real(vec![0x0a, 0x06, 0xff]))]; // 0.06
}

const ISO_ADOBE_LAST_SID: u16 = 228;
const ADOBE: &[u8] = b"Adobe";
const IDENTITY: &[u8] = b"Identity";

/// Top level representation of a CFF font file, typically read from a CFF OpenType table.
///
/// Refer to Technical Note #5176
#[derive(Clone)]
pub struct CFF<'a> {
    pub header: Header,
    pub name_index: Index<'a>,
    pub string_index: MaybeOwnedIndex<'a>,
    pub global_subr_index: Index<'a>,
    pub fonts: Vec<Font<'a>>,
}

/// CFF Font Header described in Section 6 of Technical Note #5176
#[derive(Clone, Debug, PartialEq)]
pub struct Header {
    pub major: u8,
    pub minor: u8,
    pub hdr_size: u8,
    pub off_size: u8,
}

/// A CFF INDEX described in Section 5 of Technical Note #5176
#[derive(Clone)]
pub struct Index<'a> {
    pub count: usize,
    off_size: u8,
    offset_array: &'a [u8],
    data_array: &'a [u8],
}

/// A single font within a CFF file
#[derive(Clone)]
pub struct Font<'a> {
    pub top_dict: TopDict,
    pub char_strings_index: MaybeOwnedIndex<'a>,
    pub charset: Charset<'a>,
    pub data: CFFVariant<'a>,
}

#[derive(Clone)]
pub enum MaybeOwnedIndex<'a> {
    Borrowed(Index<'a>),
    Owned(owned::Index),
}

pub struct MaybeOwnedIndexIterator<'a> {
    data: &'a MaybeOwnedIndex<'a>,
    index: usize,
}

mod owned {
    use super::{TryFrom, U16Be, WriteBinary, WriteContext, WriteError, U8};

    #[derive(Clone)]
    pub struct Index {
        pub(super) data: Vec<Vec<u8>>,
    }

    impl WriteBinary<&Self> for Index {
        type Output = ();

        fn write<C: WriteContext>(ctxt: &mut C, index: &Index) -> Result<(), WriteError> {
            let count = u16::try_from(index.data.len())?;
            U16Be::write(ctxt, count)?;
            if count == 0 {
                return Ok(());
            }

            let mut offset = 1; // INDEX offsets start at 1
            let mut offsets = Vec::with_capacity(index.data.len() + 1);
            for data in &index.data {
                offsets.push(offset);
                offset += data.len();
            }
            offsets.push(offset);
            let (off_size, offset_array) = super::serialise_offset_array(offsets)?;
            U8::write(ctxt, off_size)?;
            ctxt.write_bytes(&offset_array)?;
            for data in &index.data {
                ctxt.write_bytes(data)?;
            }

            Ok(())
        }
    }

    impl Index {
        pub(super) fn read_object(&self, index: usize) -> Option<&[u8]> {
            self.data.get(index).map(|data| data.as_slice())
        }
    }
}

#[derive(Clone)]
pub enum CFFVariant<'a> {
    CID(CIDData<'a>),
    Type1(Type1Data<'a>),
}

#[derive(Clone)]
pub struct CIDData<'a> {
    pub font_dict_index: MaybeOwnedIndex<'a>,
    pub private_dicts: Vec<PrivateDict>,
    pub local_subr_indices: Vec<Option<Index<'a>>>,
    pub fd_select: FDSelect<'a>,
}

pub struct CIDDataOffsets {
    pub font_dict_index: usize,
    pub fd_select: usize,
}

#[derive(Clone)]
pub struct Type1Data<'a> {
    pub encoding: Encoding<'a>,
    pub private_dict: PrivateDict,
    pub local_subr_index: Option<Index<'a>>,
}

pub struct Type1DataOffsets {
    pub custom_encoding: Option<usize>,
    pub private_dict: usize,
    pub private_dict_len: usize,
}

// Encoding data is located via the offset operand to the Encoding operator in the Top DICT. Only
// one Encoding operator can be specified per font except for CIDFonts which specify no encoding.
#[derive(Clone)]
pub enum Encoding<'a> {
    Standard,
    Expert,
    Custom(CustomEncoding<'a>),
}

#[derive(Clone)]
pub enum Charset<'a> {
    ISOAdobe,
    Expert,
    ExpertSubset,
    Custom(CustomCharset<'a>),
}

#[derive(Clone)]
pub enum CustomEncoding<'a> {
    Format0 {
        codes: ReadArray<'a, U8>,
    },
    Format1 {
        ranges: ReadArray<'a, Range<u8, u8>>,
    },
}

// A string id in the font
type SID = u16;

#[derive(Clone)]
pub enum CustomCharset<'a> {
    Format0 {
        glyphs: ReadArrayCow<'a, U16Be>,
    },
    Format1 {
        ranges: ReadArrayCow<'a, Range<SID, u8>>,
    },
    Format2 {
        ranges: ReadArrayCow<'a, Range<SID, u16>>,
    },
}

/// A Range from `first` to `first + n_left`
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Range<F, N> {
    pub first: F,
    pub n_left: N,
}

/// A CFF DICT described in Section 4 of Technical Note #5176
#[derive(Debug, PartialEq, Clone)]
pub struct Dict<T>
where
    T: DictDefault,
{
    dict: Vec<(Operator, Vec<Operand>)>,
    default: PhantomData<T>,
}

/// The default values of a DICT
pub trait DictDefault {
    /// Returns the default operand(s) if any for the supplied `op`.
    fn default(op: Operator) -> Option<&'static [Operand]>;
}

#[derive(Debug, PartialEq, Clone)]
pub struct TopDictDefault;

#[derive(Debug, PartialEq, Clone)]
pub struct FontDictDefault;

#[derive(Debug, PartialEq, Clone)]
pub struct PrivateDictDefault;

pub type TopDict = Dict<TopDictDefault>;

pub type FontDict = Dict<FontDictDefault>;

pub type PrivateDict = Dict<PrivateDictDefault>;

/// A collection of offset changes to a `Dict`
///
/// `DictDelta` only accepts Operators with offsets as operands.
#[derive(Debug, PartialEq, Clone)]
pub struct DictDelta {
    dict: Vec<(Operator, Vec<Operand>)>,
}

/// Font DICT select as described in Section 19 of Technical Note #5176
#[derive(Clone, Debug)]
pub enum FDSelect<'a> {
    Format0 {
        glyph_font_dict_indices: ReadArrayCow<'a, U8>,
    },
    // Formats 1 and 2 are not defined
    Format3 {
        ranges: ReadArrayCow<'a, Range<u16, u8>>,
        sentinel: u16,
    },
}

/// CFF DICT operator
#[derive(Debug, PartialEq)]
enum Op {
    Operator(Operator),
    Operand(Operand),
}

/// CFF operand to an operator
#[derive(Debug, PartialEq, Clone)]
pub enum Operand {
    Integer(i32),
    Offset(i32),
    Real(Real),
}

// This representation of real values seems a little sub-optimal since most values are likely to be
// only a few bytes. In practice we probably won't need to handle many of these values so it's
// probably not an issue. If it does impact performance, perhaps consider using the smallvec crate.
#[derive(Debug, PartialEq, Clone)]
pub struct Real(Vec<u8>);

#[repr(u16)]
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum Operator {
    Version = 0,
    Notice = 1,
    FullName = 2,
    FamilyName = 3,
    Weight = 4,
    FontBBox = 5,
    BlueValues = 6,
    OtherBlues = 7,
    FamilyBlues = 8,
    FamilyOtherBlues = 9,
    StdHW = 10,
    StdVW = 11,
    UniqueID = 13,
    XUID = 14,
    Charset = 15,
    Encoding = 16,
    CharStrings = 17,
    Private = 18,
    Subrs = 19,
    DefaultWidthX = 20,
    NominalWidthX = 21,
    Copyright = op2(0),
    IsFixedPitch = op2(1),
    ItalicAngle = op2(2),
    UnderlinePosition = op2(3),
    UnderlineThickness = op2(4),
    PaintType = op2(5),
    CharstringType = op2(6),
    FontMatrix = op2(7),
    StrokeWidth = op2(8),
    BlueScale = op2(9),
    BlueShift = op2(10),
    BlueFuzz = op2(11),
    StemSnapH = op2(12),
    StemSnapV = op2(13),
    ForceBold = op2(14),
    LanguageGroup = op2(17),
    ExpansionFactor = op2(18),
    InitialRandomSeed = op2(19),
    SyntheticBase = op2(20),
    PostScript = op2(21),
    BaseFontName = op2(22),
    BaseFontBlend = op2(23),
    ROS = op2(30),
    CIDFontVersion = op2(31),
    CIDFontRevision = op2(32),
    CIDFontType = op2(33),
    CIDCount = op2(34),
    UIDBase = op2(35),
    FDArray = op2(36),
    FDSelect = op2(37),
    FontName = op2(38),
}

const fn op2(value: u8) -> u16 {
    (12 << 8) | (value as u16)
}

impl<'a> ReadBinary<'a> for CFF<'a> {
    type HostType = Self;

    fn read(ctxt: &mut ReadCtxt<'a>) -> Result<Self, ParseError> {
        // Get a scope that starts at the beginning of the CFF data. This is needed for reading
        // data that is specified as an offset from the start of the data later.
        let scope = ctxt.scope();

        let header = ctxt.read::<Header>()?;
        let name_index = ctxt.read::<Index<'_>>()?;
        let top_dict_index = ctxt.read::<Index<'_>>()?;
        let string_index = ctxt.read::<Index<'_>>()?;
        let global_subr_index = ctxt.read::<Index<'_>>()?;

        let mut fonts = Vec::with_capacity(name_index.count);
        for font_index in 0..name_index.count {
            let top_dict = top_dict_index.read::<TopDict>(font_index)?;

            // CharStrings index
            let offset = top_dict
                .get_i32(Operator::CharStrings)
                .unwrap_or(Err(ParseError::MissingValue))?;
            let char_strings_index = scope.offset(usize::try_from(offset)?).read::<Index<'_>>()?;

            // The Top DICT begins with the SyntheticBase and ROS operators
            // for synthetic and CIDFonts, respectively. Regular Type 1 fonts
            // begin with some other operator.
            let data = match top_dict.first_operator() {
                Some(Operator::ROS) => {
                    let cid_data = read_cid_data(&scope, &top_dict, char_strings_index.count)?;
                    CFFVariant::CID(cid_data)
                }
                Some(Operator::SyntheticBase) => {
                    return Err(ParseError::NotImplemented);
                }
                Some(_) => {
                    let (private_dict, private_dict_offset) = top_dict.read_private_dict(&scope)?;
                    let local_subr_index =
                        read_local_subr_index(&scope, &private_dict, private_dict_offset)?;
                    let encoding = read_encoding(&scope, &top_dict)?;

                    CFFVariant::Type1(Type1Data {
                        encoding,
                        private_dict,
                        local_subr_index,
                    })
                }
                None => return Err(ParseError::MissingValue),
            };

            let charset = read_charset(&scope, &top_dict, char_strings_index.count)?;

            fonts.push(Font {
                top_dict,
                char_strings_index: MaybeOwnedIndex::Borrowed(char_strings_index),
                charset,
                data,
            });
        }

        Ok(CFF {
            header,
            name_index,
            string_index: MaybeOwnedIndex::Borrowed(string_index),
            global_subr_index,
            fonts,
        })
    }
}

impl<'a> WriteBinary<&Self> for CFF<'a> {
    type Output = ();

    fn write<C: WriteContext>(ctxt: &mut C, cff: &CFF<'a>) -> Result<(), WriteError> {
        Header::write(ctxt, &cff.header)?;
        Index::write(ctxt, &cff.name_index)?;
        let top_dicts = cff.fonts.iter().map(|font| &font.top_dict).collect_vec();
        let top_dict_index_length =
            Index::calculate_size::<TopDict, _>(top_dicts.as_slice(), DictDelta::new())?;
        let top_dict_index_placeholder = ctxt.reserve::<Index<'_>, _>(top_dict_index_length)?;
        MaybeOwnedIndex::write(ctxt, &cff.string_index)?;
        Index::write(ctxt, &cff.global_subr_index)?;

        // Collect Top DICT deltas now that we know the offsets to other items in the DICT
        let mut top_dict_deltas = vec![DictDelta::new(); cff.fonts.len()];
        for (font, mut top_dict_delta) in cff.fonts.iter().zip(top_dict_deltas.iter_mut()) {
            top_dict_delta.push_offset(Operator::CharStrings, i32::try_from(ctxt.bytes_written())?);
            MaybeOwnedIndex::write(ctxt, &font.char_strings_index)?;

            match &font.charset {
                Charset::ISOAdobe => top_dict_delta.push_offset(Operator::Charset, 0),
                Charset::Expert => top_dict_delta.push_offset(Operator::Charset, 1),
                Charset::ExpertSubset => top_dict_delta.push_offset(Operator::Charset, 2),
                Charset::Custom(custom) => {
                    top_dict_delta
                        .push_offset(Operator::Charset, i32::try_from(ctxt.bytes_written())?);
                    CustomCharset::write(ctxt, custom)?;
                }
            }
            write_cff_variant(ctxt, &font.data, &mut top_dict_delta)?;
        }

        // Write out the Top DICTs with the updated offsets
        let mut top_dict_data = WriteBuffer::new();
        let mut offsets = Vec::with_capacity(cff.fonts.len());
        for (font, top_dict_delta) in cff.fonts.iter().zip(top_dict_deltas.into_iter()) {
            offsets.push(top_dict_data.bytes_written() + 1); // +1 because INDEX offsets start at 1
            TopDict::write_dep(&mut top_dict_data, &font.top_dict, top_dict_delta)?;
        }
        offsets.push(top_dict_data.bytes_written() + 1); // Add the extra offset at the end
        let (off_size, offset_array) = serialise_offset_array(offsets)?;

        // Fill in the Top DICT INDEX placeholder
        let top_dict_index = Index {
            count: cff.fonts.len(),
            off_size,
            offset_array: &offset_array,
            data_array: top_dict_data.bytes(),
        };
        ctxt.write_placeholder(top_dict_index_placeholder, &top_dict_index)?;

        Ok(())
    }
}

impl<'a> CFF<'a> {
    /// Read a string with the given SID from the String INDEX
    pub fn read_string(&self, sid: SID) -> Result<String, ParseError> {
        read_string_index_string(&self.string_index, sid)
    }

    /// The `Vec<u16>` in the output is a mapping from new to old glyph id.
    ///
    /// `glpyh_ids` contains the ids of the glyphs to retain.
    /// When subsetting a Type 1 CFF font and retaining more than 255 glyphs the
    /// `convert_cff_to_cid_if_more_than_255_glyphs` argument controls whether the Type 1 font
    /// is converted to a CID keyed font in the process. The primary motivation for this is
    /// broader compatibility, especially if the subset font is embedded in a PDF.
    ///
    /// **Known Limitations**
    ///
    /// Currently the subsetting process does not produce the smallest possible output font.
    /// There are various parts of the source font that are copied to the output font as-is.
    /// Specifically the subsetting process does not subset the String INDEX, or the Local or
    /// Global subroutines.
    ///
    /// Subsetting the String INDEX requires updating all String IDs (SID) in the font so
    /// that they point at their new position in the String INDEX. Subsetting the subroutines
    /// requires parsing the CharStrings, which describe the glyph outlines. The CharStrings
    /// format is non-trivial so this has been left for now.
    pub fn subset(
        &self,
        glyph_ids: &[u16],
        convert_cff_to_cid_if_more_than_255_glyphs: bool,
    ) -> Result<(Self, Vec<u16>), ParseError> {
        let mut cff = self.to_owned();
        let font: &mut Font<'_> = &mut cff.fonts[0];
        let mut charset = Vec::with_capacity(glyph_ids.len());
        let mut fd_select = Vec::with_capacity(glyph_ids.len());
        let mut new_to_old_id = Vec::with_capacity(glyph_ids.len());
        let mut glyph_data = Vec::with_capacity(glyph_ids.len());

        for &glyph_id in glyph_ids {
            let data = font
                .char_strings_index
                .read_object(usize::from(glyph_id))
                .ok_or(ParseError::BadIndex)?;
            glyph_data.push(data.to_owned());
            new_to_old_id.push(glyph_id);

            if glyph_id != 0 {
                let sid_or_cid = font
                    .charset
                    .id_for_glyph(glyph_id)
                    .ok_or(ParseError::BadIndex)?;
                charset.push(sid_or_cid);
            }

            // Calculate CID/Type 1 specific updates
            match &font.data {
                CFFVariant::CID(cid) => {
                    // Find out which font DICT this glyph maps to if it's a CID font
                    // Need to know which font DICT applies to each glyph, then ideally work out which FDSelect
                    // format is the best to use. For now it's probably good enough to just use format 0
                    let fd_index = cid
                        .fd_select
                        .font_dict_index(glyph_id)
                        .ok_or(ParseError::BadIndex)?;
                    fd_select.push(fd_index);
                }
                CFFVariant::Type1(_type1) => {}
            }
        }

        font.char_strings_index = MaybeOwnedIndex::Owned(owned::Index { data: glyph_data });

        if font.is_cid_keyed() {
            // Update CID/Type 1 specific structures
            match &mut font.data {
                CFFVariant::CID(cid) => {
                    cid.fd_select = FDSelect::Format0 {
                        glyph_font_dict_indices: ReadArrayCow::Owned(fd_select),
                    }
                }
                CFFVariant::Type1(_type1) => {}
            }

            font.charset = Charset::Custom(CustomCharset::Format0 {
                glyphs: ReadArrayCow::Owned(charset),
            });
        } else if convert_cff_to_cid_if_more_than_255_glyphs && font.char_strings_index.len() > 255
        {
            font.charset = convert_type1_to_cid(&mut cff.string_index, font)?;
        } else {
            let iso_adobe = 1..=ISO_ADOBE_LAST_SID;
            if charset
                .iter()
                .zip(iso_adobe)
                .all(|(sid, iso_adobe_sid)| *sid == iso_adobe_sid)
            {
                // As per section 18 of Technical Note #5176: There are no predefined charsets for CID
                // fonts. So this branch is only taken for Type 1 fonts.
                font.charset = Charset::ISOAdobe;
            } else {
                font.charset = Charset::Custom(CustomCharset::Format0 {
                    glyphs: ReadArrayCow::Owned(charset),
                });
            }
        }

        Ok((cff, new_to_old_id))
    }
}

/// Read a string with the given SID from the String INDEX
fn read_string_index_string<'a>(
    string_index: &MaybeOwnedIndex<'a>,
    sid: SID,
) -> Result<String, ParseError> {
    let sid = usize::from(sid);
    // When the client needs to determine the string that corresponds to a particular SID it
    // performs the following: test if SID is in standard range then fetch from internal table,
    // otherwise, fetch string from the String INDEX using a value of (SID – nStdStrings) as
    // the index
    if let Some(string) = STANDARD_STRINGS.get(sid) {
        Ok(string.to_string())
    } else {
        let bytes = string_index
            .read_object(sid - STANDARD_STRINGS.len())
            .ok_or(ParseError::BadIndex)?;

        String::from_utf8(bytes.to_vec()).map_err(|_utf8_err| ParseError::BadValue)
    }
}

fn convert_type1_to_cid<'a>(
    string_index: &mut MaybeOwnedIndex<'a>,
    font: &mut Font<'a>,
) -> Result<Charset<'a>, ParseError> {
    assert!(!font.is_cid_keyed());

    // Retrieve the SIDs of Adobe and Identity, adding them if they're not in the String INDEX
    // already.
    let (adobe_sid, identity_sid) = match (string_index.index(ADOBE), string_index.index(IDENTITY))
    {
        (Some(adobe_sid), Some(identity_sid)) => (adobe_sid, identity_sid),
        (Some(adobe_sid), None) => (adobe_sid, string_index.push(IDENTITY.to_owned())),
        (None, Some(identity_sid)) => (string_index.push(ADOBE.to_owned()), identity_sid),
        (None, None) => (
            string_index.push(ADOBE.to_owned()),
            string_index.push(IDENTITY.to_owned()),
        ),
    };

    // Build Font DICT
    let mut font_dict = FontDict::new();
    font_dict
        .inner_mut()
        .push((Operator::Private, OFFSET_ZERO.to_vec())); // Offset will be updated when written out

    let mut font_dict_buffer = WriteBuffer::new();
    FontDict::write_dep(&mut font_dict_buffer, &font_dict, DictDelta::new())
        .map_err(|_err| ParseError::BadValue)?;
    let font_dict_index = MaybeOwnedIndex::Owned(owned::Index {
        data: vec![font_dict_buffer.into_inner()],
    });

    let n_glyphs = u16::try_from(font.char_strings_index.len())?;

    let fd_select = FDSelect::Format3 {
        ranges: ReadArrayCow::Owned(vec![Range {
            first: 0,
            n_left: 0,
        }]),
        sentinel: n_glyphs,
    };
    let cid_data = CFFVariant::CID(CIDData {
        font_dict_index,
        private_dicts: Vec::new(),
        local_subr_indices: Vec::new(),
        fd_select,
    });

    // Swap Type1 data with CID data
    let type1_data = match mem::replace(&mut font.data, cid_data) {
        CFFVariant::Type1(data) => data,
        CFFVariant::CID(_) => unreachable!(),
    };
    match &mut font.data {
        CFFVariant::Type1(_type1) => unreachable!(),
        CFFVariant::CID(cid) => {
            cid.private_dicts = vec![type1_data.private_dict];
            cid.local_subr_indices = vec![type1_data.local_subr_index];
        }
    };

    // Update the Top DICT
    // Add ROS
    let registry = Operand::Integer(i32::try_from(adobe_sid)?);
    let ordering = Operand::Integer(i32::try_from(identity_sid)?);
    let supplement = Operand::Integer(0);
    let ros = (Operator::ROS, vec![registry, ordering, supplement]);
    font.top_dict.inner_mut().insert(0, ros);

    // Add FDSelect and FDArray offsets to Top DICT
    // Actual offsets will be filled in when writing
    font.top_dict
        .inner_mut()
        .push((Operator::FDArray, OFFSET_ZERO.to_vec()));
    font.top_dict
        .inner_mut()
        .push((Operator::FDSelect, OFFSET_ZERO.to_vec()));

    // Add CIDCount
    font.top_dict.inner_mut().push((
        Operator::CIDCount,
        vec![Operand::Integer(i32::from(n_glyphs))],
    ));

    // Remove Private DICT offset and encoding
    font.top_dict.remove(Operator::Private);
    font.top_dict.remove(Operator::Encoding);

    // Add charset
    Ok(Charset::Custom(CustomCharset::Format2 {
        ranges: ReadArrayCow::Owned(vec![Range {
            first: 1,
            n_left: n_glyphs.checked_sub(2).ok_or(ParseError::BadIndex)?,
        }]),
    }))
}

impl<'a> ReadBinary<'a> for Header {
    type HostType = Self;

    fn read(ctxt: &mut ReadCtxt<'a>) -> Result<Self, ParseError> {
        // From section 6 of Technical Note #5176:
        // Implementations reading font set files must include code to check version numbers so
        // that if and when the format and therefore the version number changes, older
        // implementations will reject newer versions gracefully. If the major version number is
        // understood by an implementation it can safely proceed with reading the font. The minor
        // version number indicates extensions to the format that are undetectable by
        // implementations that do not support them although they will be unable to take advantage
        // of these extensions.
        let major = ctxt.read_u8()?;
        ctxt.check(major == 1)?;
        let minor = ctxt.read_u8()?;
        let hdr_size = ctxt.read_u8()?;
        let off_size = ctxt.read_u8()?;

        if hdr_size < 4 {
            return Err(ParseError::BadValue);
        }

        if off_size < 1 || off_size > 4 {
            return Err(ParseError::BadValue);
        }

        let _unknown = ctxt.read_slice((hdr_size - 4) as usize)?;

        Ok(Header {
            major,
            minor,
            hdr_size,
            off_size,
        })
    }
}

impl WriteBinary<&Self> for Header {
    type Output = ();

    fn write<C: WriteContext>(ctxt: &mut C, header: &Header) -> Result<(), WriteError> {
        U8::write(ctxt, header.major)?;
        U8::write(ctxt, header.minor)?;
        // Any data between the header and the Name INDEX will have been discarded.
        // So the size will always be 4 bytes.
        U8::write(ctxt, 4)?; // hdr_size
        U8::write(ctxt, header.off_size)?;

        Ok(())
    }
}

impl<'a> ReadBinary<'a> for Index<'a> {
    type HostType = Self;

    fn read(ctxt: &mut ReadCtxt<'a>) -> Result<Self, ParseError> {
        let count = usize::from(ctxt.read_u16be()?);

        if count > 0 {
            let off_size = ctxt.read_u8()?;
            if off_size < 1 || off_size > 4 {
                return Err(ParseError::BadValue);
            }

            let offset_array_size = (count + 1) * usize::from(off_size);
            let offset_array = ctxt.read_slice(offset_array_size)?;

            let last_offset_index = lookup_offset_index(off_size, offset_array, count);
            if last_offset_index < 1 {
                return Err(ParseError::BadValue);
            }

            let data_array_size = last_offset_index - 1;
            let data_array = ctxt.read_slice(data_array_size)?;

            Ok(Index {
                count,
                off_size,
                offset_array,
                data_array,
            })
        } else {
            // count == 0
            Ok(Index {
                count,
                off_size: 1,
                offset_array: &[],
                data_array: &[],
            })
        }
    }
}

impl<'a> WriteBinary<&Self> for Index<'a> {
    type Output = ();

    fn write<C: WriteContext>(ctxt: &mut C, index: &Index<'a>) -> Result<(), WriteError> {
        U16Be::write(ctxt, u16::try_from(index.count)?)?;
        if index.count == 0 {
            return Ok(());
        }

        U8::write(ctxt, index.off_size)?;
        ctxt.write_bytes(index.offset_array)?;
        ctxt.write_bytes(index.data_array)?;

        Ok(())
    }
}

impl<'a> WriteBinary<&Self> for MaybeOwnedIndex<'a> {
    type Output = ();

    fn write<C: WriteContext>(ctxt: &mut C, index: &MaybeOwnedIndex<'a>) -> Result<(), WriteError> {
        match index {
            MaybeOwnedIndex::Borrowed(index) => Index::write(ctxt, index),
            MaybeOwnedIndex::Owned(index) => owned::Index::write(ctxt, index),
        }
    }
}

impl<'a, T> ReadBinary<'a> for Dict<T>
where
    T: DictDefault,
{
    type HostType = Self;

    fn read(ctxt: &mut ReadCtxt<'a>) -> Result<Self::HostType, ParseError> {
        let mut dict = Vec::new();
        let mut operands = Vec::new();

        while ctxt.bytes_available() {
            match Op::read(ctxt)? {
                Op::Operator(operator) => {
                    integer_to_offset(operator, &mut operands);
                    dict.push((operator, operands.clone()));
                    operands.clear();
                }
                Op::Operand(operand) => {
                    operands.push(operand);
                    if operands.len() > MAX_OPERANDS {
                        return Err(ParseError::LimitExceeded);
                    }
                }
            }
        }

        Ok(Dict {
            dict,
            default: PhantomData,
        })
    }
}

fn offset_size(value: usize) -> Option<u8> {
    match value {
        0..=0xFF => Some(1),
        0x100..=0xFFFF => Some(2),
        0x1_0000..=0xFF_FFFF => Some(3),
        0x100_0000..=0xFFFF_FFFF => Some(4),
        _ => None,
    }
}

// Special case handling for operands that are offsets. This function swaps them from an
// Integer to an Offset. This is later used when writing operands.
fn integer_to_offset(operator: Operator, operands: &mut [Operand]) {
    match (operator, operands.as_ref()) {
        // Encodings 0..=1 indicate predefined encodings and are not offsets
        (Operator::Encoding, [Operand::Integer(offset)]) if *offset > 1 => {
            operands[0] = Operand::Offset(*offset);
        }
        (Operator::Charset, [Operand::Integer(offset)])
        | (Operator::CharStrings, [Operand::Integer(offset)])
        | (Operator::Subrs, [Operand::Integer(offset)])
        | (Operator::FDArray, [Operand::Integer(offset)])
        | (Operator::FDSelect, [Operand::Integer(offset)]) => {
            operands[0] = Operand::Offset(*offset);
        }
        (Operator::Private, [Operand::Integer(length), Operand::Integer(offset)]) => {
            let offset = *offset; // This is a work around an ownership issue
            operands[0] = Operand::Offset(*length);
            operands[1] = Operand::Offset(offset);
        }
        _ => {}
    }
}

impl<T> WriteBinaryDep<&Self> for Dict<T>
where
    T: DictDefault,
{
    type Args = DictDelta;
    type Output = usize; // The length of the written Dict

    fn write_dep<C: WriteContext>(
        ctxt: &mut C,
        dict: &Dict<T>,
        delta: DictDelta,
    ) -> Result<Self::Output, WriteError> {
        let offset = ctxt.bytes_written();

        for (operator, operands) in dict.iter() {
            let mut operands = operands.as_slice();

            // Replace operands with delta operands if present otherwise skip if operands match
            // default. We never skip operands pulled from the delta DICT as these are offsets and
            // always need to be written in order to make the size of the DICT predictable.
            if let Some(delta_operands) = delta.get(*operator) {
                operands = delta_operands;
            } else if T::default(*operator)
                .map(|defaults| defaults == operands)
                .unwrap_or(false)
            {
                continue;
            }

            for operand in operands {
                Operand::write(ctxt, operand)?;
            }
            Operator::write(ctxt, *operator)?;
        }

        Ok(ctxt.bytes_written() - offset)
    }
}

impl<'a> ReadBinary<'a> for Op {
    type HostType = Self;

    fn read(ctxt: &mut ReadCtxt<'a>) -> Result<Self, ParseError> {
        let b0 = ctxt.read_u8()?;

        match b0 {
            0..=11 | 13..=21 => ok_operator(u16::from(b0).try_into().unwrap()), // NOTE(unwrap): Safe due to pattern
            12 => ok_operator(u16::from(op2(ctxt.read_u8()?)).try_into()?),
            28 => {
                let num = ctxt.read_i16be()?;
                Ok(Op::Operand(Operand::Integer(i32::from(num))))
            }
            29 => ok_int(ctxt.read_i32be()?),
            30 => ok_real(ctxt.read_until_nibble(0xF)?),
            32..=246 => ok_int(i32::from(b0) - 139),
            247..=250 => {
                let b1 = ctxt.read_u8()?;
                ok_int((i32::from(b0) - 247) * 256 + i32::from(b1) + 108)
            }
            251..=254 => {
                let b1 = ctxt.read_u8()?;
                ok_int(-(i32::from(b0) - 251) * 256 - i32::from(b1) - 108)
            }
            22..=27 | 31 | 255 => Err(ParseError::BadValue), // reserved
        }
    }
}

impl WriteBinary<Self> for Operator {
    type Output = ();

    fn write<C: WriteContext>(ctxt: &mut C, op: Operator) -> Result<(), WriteError> {
        let value = op as u16;
        if value > 0xFF {
            U16Be::write(ctxt, value)?;
        } else {
            U8::write(ctxt, value as u8)?;
        }

        Ok(())
    }
}

impl WriteBinary<&Self> for Operand {
    type Output = ();

    // Refer to Table 3 Operand Encoding in section 4 of Technical Note #5176 for details on the
    // integer encoding scheme.
    fn write<C: WriteContext>(ctxt: &mut C, op: &Operand) -> Result<(), WriteError> {
        match op {
            Operand::Integer(val) => match *val {
                // NOTE: Casts are safe due to patterns limiting range
                -107..=107 => {
                    U8::write(ctxt, (val + 139) as u8)?;
                }
                108..=1131 => {
                    let val = *val - 108;
                    U8::write(ctxt, ((val >> 8) + 247) as u8)?;
                    U8::write(ctxt, val as u8)?;
                }
                -1131..=-108 => {
                    let val = -*val - 108;
                    U8::write(ctxt, ((val >> 8) + 251) as u8)?;
                    U8::write(ctxt, val as u8)?;
                }
                -32768..=32767 => {
                    U8::write(ctxt, 28)?;
                    I16Be::write(ctxt, *val as i16)?
                }
                _ => {
                    U8::write(ctxt, 29)?;
                    I32Be::write(ctxt, *val)?
                }
            },
            Operand::Offset(val) => {
                U8::write(ctxt, 29)?;
                // Offsets are always encoded using the i32 representation to make their size
                // predictable.
                I32Be::write(ctxt, *val)?;
            }
            Operand::Real(Real(val)) => {
                U8::write(ctxt, 30)?;
                ctxt.write_bytes(val)?;
            }
        }

        Ok(())
    }
}

fn ok_operator(op: Operator) -> Result<Op, ParseError> {
    Ok(Op::Operator(op))
}

fn ok_int(num: i32) -> Result<Op, ParseError> {
    Ok(Op::Operand(Operand::Integer(num)))
}

fn ok_real(slice: &[u8]) -> Result<Op, ParseError> {
    Ok(Op::Operand(Operand::Real(Real(slice.to_owned()))))
}

impl<'a> ReadFrom<'a> for Range<u8, u8> {
    type ReadType = (U8, U8);
    fn from((first, n_left): (u8, u8)) -> Self {
        Range { first, n_left }
    }
}

impl<'a> WriteBinary for Range<u8, u8> {
    type Output = ();

    fn write<C: WriteContext>(ctxt: &mut C, range: Self) -> Result<(), WriteError> {
        U8::write(ctxt, range.first)?;
        U8::write(ctxt, range.n_left)?;

        Ok(())
    }
}

impl<'a> ReadFrom<'a> for Range<SID, u8> {
    type ReadType = (U16Be, U8);
    fn from((first, n_left): (SID, u8)) -> Self {
        Range { first, n_left }
    }
}

impl<'a> WriteBinary for Range<SID, u8> {
    type Output = ();

    fn write<C: WriteContext>(ctxt: &mut C, range: Self) -> Result<(), WriteError> {
        U16Be::write(ctxt, range.first)?;
        U8::write(ctxt, range.n_left)?;

        Ok(())
    }
}

impl<'a> ReadFrom<'a> for Range<SID, u16> {
    type ReadType = (U16Be, U16Be);
    fn from((first, n_left): (SID, u16)) -> Self {
        Range { first, n_left }
    }
}

impl<'a> WriteBinary for Range<SID, u16> {
    type Output = ();

    fn write<C: WriteContext>(ctxt: &mut C, range: Self) -> Result<(), WriteError> {
        U16Be::write(ctxt, range.first)?;
        U16Be::write(ctxt, range.n_left)?;

        Ok(())
    }
}

impl<'a, F, N> Range<F, N>
where
    N: num::Unsigned + Copy,
    usize: From<N>,
{
    pub fn len(&self) -> usize {
        usize::from(self.n_left) + 1
    }
}

// TODO: Make these generic. Requires Rust stabilisation of the Step trait or its replacement.
// https://doc.rust-lang.org/core/iter/trait.Step.html
impl<'a> Range<SID, u8> {
    pub fn iter(&self) -> impl Iterator<Item = SID> {
        let last = self.first + SID::from(self.n_left);
        self.first..=last
    }
}

impl<'a> Range<SID, u16> {
    pub fn iter(&self) -> impl Iterator<Item = SID> {
        let last = self.first + self.n_left;
        self.first..=last
    }
}

impl<'a> ReadBinary<'a> for CustomEncoding<'a> {
    type HostType = Self;

    fn read(ctxt: &mut ReadCtxt<'a>) -> Result<Self, ParseError> {
        match ctxt.read::<U8>()? {
            0 => {
                let ncodes = ctxt.read::<U8>()?;
                let codes = ctxt.read_array::<U8>(usize::from(ncodes))?;
                Ok(CustomEncoding::Format0 { codes })
            }
            1 => {
                let nranges = ctxt.read::<U8>()?;
                let ranges = ctxt.read_array::<Range<u8, u8>>(usize::from(nranges))?;
                Ok(CustomEncoding::Format1 { ranges })
            }
            _ => Err(ParseError::BadValue),
        }
    }
}

impl<'a> WriteBinary<&Self> for CustomEncoding<'a> {
    type Output = ();

    fn write<C: WriteContext>(ctxt: &mut C, encoding: &Self) -> Result<(), WriteError> {
        match encoding {
            CustomEncoding::Format0 { codes } => {
                U8::write(ctxt, 0)?; // format
                U8::write(ctxt, u8::try_from(codes.len())?)?;
                <&ReadArray<'_, _>>::write(ctxt, codes)?;
            }
            CustomEncoding::Format1 { ranges } => {
                U8::write(ctxt, 1)?; // format
                U8::write(ctxt, u8::try_from(ranges.len())?)?;
                <&ReadArray<'_, _>>::write(ctxt, ranges)?;
            }
        }

        Ok(())
    }
}

impl<'a> Charset<'a> {
    /// Returns the id of the SID (Type 1 font) or CID (CID keyed font) of the name of the supplied glyph
    pub fn id_for_glyph(&self, glyph_id: u16) -> Option<u16> {
        match self {
            // In ISOAdobe glyph ID maps to SID
            Charset::ISOAdobe => {
                if glyph_id <= ISO_ADOBE_LAST_SID {
                    Some(glyph_id)
                } else {
                    None
                }
            }
            Charset::Expert => EXPERT_CHARSET.get(usize::from(glyph_id)).cloned(),
            Charset::ExpertSubset => EXPERT_SUBSET_CHARSET.get(usize::from(glyph_id)).cloned(),
            Charset::Custom(custom) => custom.id_for_glyph(glyph_id),
        }
    }
}

impl<'a> ReadBinaryDep<'a> for CustomCharset<'a> {
    type Args = usize;
    type HostType = Self;

    fn read_dep(ctxt: &mut ReadCtxt<'a>, n_glyphs: usize) -> Result<Self, ParseError> {
        // (There is one less element in the charset than nGlyphs because the .notdef glyph name is omitted.)
        let n_glyphs = n_glyphs.checked_sub(1).ok_or(ParseError::BadValue)?;
        match ctxt.read::<U8>()? {
            0 => {
                // The number of glyphs (nGlyphs) is the value of the count field in the
                // CharStrings INDEX.
                let glyphs = ctxt.read_array::<U16Be>(n_glyphs)?;
                Ok(CustomCharset::Format0 {
                    glyphs: ReadArrayCow::Borrowed(glyphs),
                })
            }
            1 => {
                let ranges = read_range_array(ctxt, n_glyphs)?;
                Ok(CustomCharset::Format1 {
                    ranges: ReadArrayCow::Borrowed(ranges),
                })
            }
            2 => {
                let ranges = read_range_array(ctxt, n_glyphs)?;
                Ok(CustomCharset::Format2 {
                    ranges: ReadArrayCow::Borrowed(ranges),
                })
            }
            _ => Err(ParseError::BadValue),
        }
    }
}

impl<'a> WriteBinary<&Self> for CustomCharset<'a> {
    type Output = ();

    fn write<C: WriteContext>(ctxt: &mut C, charset: &Self) -> Result<(), WriteError> {
        match charset {
            CustomCharset::Format0 { glyphs } => {
                U8::write(ctxt, 0)?; // format
                ReadArrayCow::write(ctxt, glyphs)?;
            }
            CustomCharset::Format1 { ranges } => {
                U8::write(ctxt, 1)?; // format
                ReadArrayCow::write(ctxt, ranges)?;
            }
            CustomCharset::Format2 { ranges } => {
                U8::write(ctxt, 2)?; // format
                ReadArrayCow::write(ctxt, ranges)?;
            }
        }

        Ok(())
    }
}

impl<'a> CustomCharset<'a> {
    pub fn iter(&'a self) -> Box<dyn Iterator<Item = u16> + 'a> {
        let notdef: Box<dyn Iterator<Item = u16>> = Box::new(iter::once(0));
        match &self {
            CustomCharset::Format0 { glyphs } => Box::new(notdef.chain(glyphs.iter())),
            CustomCharset::Format1 { ranges } => ranges
                .iter()
                .fold(notdef, |chain, range| Box::new(chain.chain(range.iter()))),
            CustomCharset::Format2 { ranges } => ranges
                .iter()
                .fold(notdef, |chain, range| Box::new(chain.chain(range.iter()))),
        }
    }

    /// Returns the SID (Type 1 font) or CID (CID keyed font) of the name of the supplied glyph
    pub fn id_for_glyph(&self, glyph_id: u16) -> Option<u16> {
        // Section 11 of Technical Note #5176:
        // By definition the first glyph (GID 0) is “.notdef” and must be present in all fonts.
        // Since this is always the case, it is not necessary to represent either the encoding
        // (unencoded) or name (.notdef) for GID 0. Consequently, taking advantage of this
        // optimization, the encoding and charset arrays always begin with GID 1.
        if glyph_id == 0 {
            return Some(0);
        }

        match self {
            CustomCharset::Format0 { glyphs } => {
                let index = usize::from(glyph_id - 1);
                glyphs
                    .check_index(index)
                    .map(|_| glyphs.get_item(index))
                    .ok()
            }
            CustomCharset::Format1 { ranges } => Self::id_for_glyph_in_ranges(ranges, glyph_id),
            CustomCharset::Format2 { ranges } => Self::id_for_glyph_in_ranges(ranges, glyph_id),
        }
    }

    fn id_for_glyph_in_ranges<F, N>(
        ranges: &ReadArrayCow<'a, Range<F, N>>,
        glyph_id: u16,
    ) -> Option<u16>
    where
        F: num::Unsigned + Copy,
        N: num::Unsigned + Copy,
        usize: From<N> + From<F>,
        Range<F, N>: ReadFrom<'a>,
        <Range<F, N> as ReadUnchecked<'a>>::HostType: Copy,
    {
        let glyph_id = usize::from(glyph_id);

        ranges
            .iter()
            .scan(0usize, |glyphs_covered, range| {
                *glyphs_covered += range.len();
                Some((*glyphs_covered, range))
            })
            .find(|(glyphs_covered, _range)| glyph_id <= *glyphs_covered)
            .and_then(|(glyphs_covered, range)| {
                (usize::from(range.first) + (glyph_id - (glyphs_covered - range.len()) - 1))
                    .try_into()
                    .ok()
            })
    }
}

impl<'a> ReadBinaryDep<'a> for FDSelect<'a> {
    type Args = usize;
    type HostType = Self;

    fn read_dep(ctxt: &mut ReadCtxt<'a>, n_glyphs: usize) -> Result<Self, ParseError> {
        match ctxt.read::<U8>()? {
            0 => {
                let glyph_font_dict_indices = ctxt.read_array::<U8>(n_glyphs)?;
                Ok(FDSelect::Format0 {
                    glyph_font_dict_indices: ReadArrayCow::Borrowed(glyph_font_dict_indices),
                })
            }
            3 => {
                let nranges = usize::from(ctxt.read::<U16Be>()?);
                let ranges = ctxt.read_array(nranges)?;
                let sentinel = ctxt.read::<U16Be>()?;
                Ok(FDSelect::Format3 {
                    ranges: ReadArrayCow::Borrowed(ranges),
                    sentinel: sentinel,
                })
            }
            _ => Err(ParseError::BadValue),
        }
    }
}

impl<'a> WriteBinary<&Self> for FDSelect<'a> {
    type Output = ();

    fn write<C: WriteContext>(ctxt: &mut C, fd_select: &Self) -> Result<(), WriteError> {
        match fd_select {
            FDSelect::Format0 {
                glyph_font_dict_indices,
            } => {
                U8::write(ctxt, 0)?; // format
                ReadArrayCow::write(ctxt, glyph_font_dict_indices)?;
            }
            FDSelect::Format3 { ranges, sentinel } => {
                U8::write(ctxt, 3)?; // format
                U16Be::write(ctxt, u16::try_from(ranges.len())?)?;
                ReadArrayCow::write(ctxt, ranges)?;
                U16Be::write(ctxt, *sentinel)?;
            }
        }

        Ok(())
    }
}

impl<'a> PartialEq for FDSelect<'a> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                FDSelect::Format0 {
                    glyph_font_dict_indices: self_glyph_font_dict_indices,
                },
                FDSelect::Format0 {
                    glyph_font_dict_indices: other_glyph_font_dict_indices,
                },
            ) => {
                self_glyph_font_dict_indices.len() == other_glyph_font_dict_indices.len()
                    && self_glyph_font_dict_indices
                        .iter()
                        .zip(other_glyph_font_dict_indices.iter())
                        .all(|(left, right)| left == right)
            }
            (
                FDSelect::Format3 {
                    ranges: self_ranges,
                    sentinel: self_sentinel,
                },
                FDSelect::Format3 {
                    ranges: other_ranges,
                    sentinel: other_sentinel,
                },
            ) => {
                self_ranges.len() == other_ranges.len()
                    && self_sentinel == other_sentinel
                    && self_ranges
                        .iter()
                        .zip(other_ranges.iter())
                        .all(|(left, right)| left == right)
            }
            _ => false,
        }
    }
}

impl<'a> FDSelect<'a> {
    /// Returns the index of the Font DICT for the supplied `glyph_id`
    pub fn font_dict_index(&self, glyph_id: u16) -> Option<u8> {
        let index = usize::from(glyph_id);
        match self {
            FDSelect::Format0 {
                glyph_font_dict_indices,
            } => glyph_font_dict_indices
                .check_index(index)
                .ok()
                .map(|_| glyph_font_dict_indices.get_item(index)),
            FDSelect::Format3 { ranges, sentinel } => {
                #[rustfmt::skip]
                let range_windows = ranges
                    .iter()
                    .map(|Range { first, n_left }| (first, Some(n_left)))
                    .chain(iter::once((*sentinel, None)))
                    .tuple_windows();

                for ((first, fd_index), (last, _)) in range_windows {
                    if glyph_id >= first && glyph_id < last {
                        return fd_index;
                    }
                }

                None
            }
        }
    }
}

impl<'a> Index<'a> {
    fn read_object(&self, index: usize) -> Option<&[u8]> {
        if index < self.count {
            let start_index = lookup_offset_index(self.off_size, self.offset_array, index) - 1;
            let end_index = lookup_offset_index(self.off_size, self.offset_array, index + 1) - 1;
            Some(&self.data_array[start_index..end_index])
        } else {
            None
        }
    }

    pub fn read<T: ReadBinaryDep<'a, Args = ()>>(
        &'a self,
        index: usize,
    ) -> Result<T::HostType, ParseError> {
        let data = self.read_object(index).ok_or(ParseError::BadIndex)?;
        ReadScope::new(data).read_dep::<T>(())
    }

    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        // NOTE(unwrap): Safe since we're iterating over valid indices
        (0..self.count).map(move |i| self.read_object(i).unwrap())
    }

    /// Returns the length required to write `objects`.
    pub fn calculate_size<'b, T, HostType>(
        objects: &'b [&HostType],
        args: T::Args,
    ) -> Result<usize, WriteError>
    where
        T: WriteBinaryDep<&'b HostType>,
        T::Args: Clone,
    {
        let mut counter = WriteCounter::new();

        U16Be::write(&mut counter, u16::try_from(objects.len())?)?;
        let off_size = if objects.len() != 0 {
            let start = counter.bytes_written();
            for obj in objects {
                T::write_dep(&mut counter, obj, args.clone())?;
            }
            let last_offset = counter.bytes_written() - start + 1; // +1 because index offsets start at 1
            let off_size = offset_size(last_offset).ok_or(WriteError::BadValue)?;
            U8::write(&mut counter, off_size)?;
            off_size
        } else {
            0
        };

        let offset_array_size = usize::from(off_size) * (objects.len() + 1);
        Ok(counter.bytes_written() + offset_array_size)
    }

    /// Returns the size of the data held by this INDEX.
    pub fn data_len(&self) -> usize {
        self.data_array.len()
    }
}

impl<'a> MaybeOwnedIndex<'a> {
    pub fn iter(&'a self) -> MaybeOwnedIndexIterator<'a> {
        MaybeOwnedIndexIterator {
            data: self,
            index: 0,
        }
    }

    pub fn read_object(&self, index: usize) -> Option<&[u8]> {
        match self {
            MaybeOwnedIndex::Borrowed(idx) => idx.read_object(index),
            MaybeOwnedIndex::Owned(idx) => idx.read_object(index),
        }
    }

    /// Returns the number of items in self.
    pub fn len(&self) -> usize {
        match self {
            MaybeOwnedIndex::Borrowed(index) => index.count,
            MaybeOwnedIndex::Owned(index) => index.data.len(),
        }
    }

    /// Returns the index of `object` in self if found.
    fn index(&self, object: &[u8]) -> Option<usize> {
        self.iter().position(|obj| obj == object)
    }

    /// Push an object onto this `MaybeOwnedIndex`. Returns the index of the object in self.
    ///
    /// If self is `Borrowed` then it is converted to the `Owned` variant first.
    fn push(&mut self, object: Vec<u8>) -> usize {
        match self {
            MaybeOwnedIndex::Borrowed(_) => {
                self.to_owned();
                self.push(object);
            }
            MaybeOwnedIndex::Owned(index) => {
                index.data.push(object);
            }
        }

        self.len() - 1
    }

    /// If self is the `Borrowed` variant, convert to the `Owned` variant.
    fn to_owned(&mut self) {
        match self {
            MaybeOwnedIndex::Borrowed(data) => {
                let data = data.iter().map(|obj| obj.to_owned()).collect();
                *self = MaybeOwnedIndex::Owned(owned::Index { data })
            }
            MaybeOwnedIndex::Owned(_) => {}
        }
    }
}

impl<'a> Iterator for MaybeOwnedIndexIterator<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.data.len() {
            let index = self.index;
            self.index += 1;
            self.data.read_object(index)
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.data.len();
        (len, Some(len))
    }
}

impl DictDefault for TopDictDefault {
    fn default(op: Operator) -> Option<&'static [Operand]> {
        match op {
            Operator::IsFixedPitch => Some(&OPERAND_ZERO),
            Operator::ItalicAngle => Some(&OPERAND_ZERO),
            Operator::UnderlinePosition => Some(&DEFAULT_UNDERLINE_POSITION),
            Operator::UnderlineThickness => Some(&DEFAULT_UNDERLINE_THICKNESS),
            Operator::PaintType => Some(&OPERAND_ZERO),
            Operator::CharstringType => Some(&DEFAULT_CHARSTRING_TYPE),
            Operator::FontMatrix => Some(DEFAULT_FONT_MATRIX.as_ref()),
            Operator::FontBBox => Some(&DEFAULT_BBOX),
            Operator::StrokeWidth => Some(&OPERAND_ZERO),
            Operator::Charset => Some(&OFFSET_ZERO),
            Operator::Encoding => Some(&OFFSET_ZERO),
            Operator::CIDFontVersion => Some(&OPERAND_ZERO),
            Operator::CIDFontRevision => Some(&OPERAND_ZERO),
            Operator::CIDFontType => Some(&OPERAND_ZERO),
            Operator::CIDCount => Some(&DEFAULT_CID_COUNT),
            _ => None,
        }
    }
}

impl DictDefault for FontDictDefault {
    fn default(_op: Operator) -> Option<&'static [Operand]> {
        None
    }
}

impl DictDefault for PrivateDictDefault {
    fn default(op: Operator) -> Option<&'static [Operand]> {
        match op {
            Operator::BlueScale => Some(DEFAULT_BLUE_SCALE.as_ref()),
            Operator::BlueShift => Some(&DEFAULT_BLUE_SHIFT),
            Operator::BlueFuzz => Some(&DEFAULT_BLUE_FUZZ),
            Operator::ForceBold => Some(&OPERAND_ZERO),
            Operator::LanguageGroup => Some(&OPERAND_ZERO),
            Operator::ExpansionFactor => Some(DEFAULT_EXPANSION_FACTOR.as_ref()),
            Operator::InitialRandomSeed => Some(&OPERAND_ZERO),
            Operator::StrokeWidth => Some(&OPERAND_ZERO),
            Operator::DefaultWidthX => Some(&OPERAND_ZERO),
            Operator::NominalWidthX => Some(&OPERAND_ZERO),
            _ => None,
        }
    }
}

impl<'a, T> Dict<T>
where
    T: DictDefault,
{
    pub fn new() -> Self {
        Dict {
            dict: Vec::new(),
            default: PhantomData,
        }
    }

    pub fn get_with_default(&self, key: Operator) -> Option<&[Operand]> {
        self.get(key).or_else(|| T::default(key))
    }

    pub fn get(&self, key: Operator) -> Option<&[Operand]> {
        self.dict
            .iter()
            .filter_map(|(op, args)| {
                if *op == key {
                    Some(args.as_slice())
                } else {
                    None
                }
            })
            .next()
    }

    /// Returns the i32 value of this operator if the operands hold a single Integer.
    pub fn get_i32(&self, key: Operator) -> Option<Result<i32, ParseError>> {
        self.get_with_default(key).map(|operands| match operands {
            [Operand::Integer(number)] => Ok(*number),
            [Operand::Offset(number)] => Ok(*number),
            _ => Err(ParseError::BadValue),
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = &(Operator, Vec<Operand>)> {
        self.dict.iter()
    }

    /// Returns the first operator of this DICT or `None` if the DICT is empty.
    pub fn first_operator(&self) -> Option<Operator> {
        match self.iter().next() {
            Some((operator, _)) => Some(*operator),
            None => None,
        }
    }

    /// Read a PrivateDict from this Dict returning it and its offset within `scope` on success.
    ///
    /// A Private DICT is required, but may be specified as having a length of 0 if there are no
    /// non-default values to be stored.
    pub fn read_private_dict(
        &self,
        scope: &ReadScope<'a>,
    ) -> Result<(PrivateDict, usize), ParseError> {
        let (private_dict_offset, private_dict_length) =
            match self.get_with_default(Operator::Private) {
                Some([Operand::Offset(length), Operand::Offset(offset)]) => {
                    Ok((usize::try_from(*offset)?, usize::try_from(*length)?))
                }
                Some(_) => Err(ParseError::BadValue),
                None => Err(ParseError::MissingValue),
            }?;
        scope
            .offset_length(private_dict_offset, private_dict_length)?
            .read::<PrivateDict>()
            .map(|dict| (dict, private_dict_offset))
    }

    pub fn len(&self) -> usize {
        self.dict.len()
    }

    fn inner_mut(&mut self) -> &mut Vec<(Operator, Vec<Operand>)> {
        &mut self.dict
    }

    fn remove(&mut self, operator: Operator) {
        if let Some(index) = self.dict.iter().position(|(op, _)| *op == operator) {
            self.dict.remove(index);
        }
    }
}

impl DictDelta {
    pub fn new() -> Self {
        DictDelta { dict: Vec::new() }
    }

    pub fn get(&self, key: Operator) -> Option<&[Operand]> {
        self.dict
            .iter()
            .filter_map(|(op, args)| {
                if *op == key {
                    Some(args.as_slice())
                } else {
                    None
                }
            })
            .next()
    }

    /// Push `operator` on this Dict as an Offset Operand
    pub fn push_offset(&mut self, operator: Operator, offset: i32) {
        self.dict.push((operator, vec![Operand::Offset(offset)]))
    }

    /// Push `operands` onto this Dict
    ///
    /// Panics if all `operands` are not `Operand::Offsets`
    pub fn push(&mut self, operator: Operator, operands: Vec<Operand>) {
        assert!(operands.iter().all(Operand::is_offset));
        self.dict.push((operator, operands))
    }
}

impl<'a> CIDData<'a> {
    pub fn font_dict(&self, index: usize) -> Result<FontDict, ParseError> {
        let data = self
            .font_dict_index
            .read_object(index)
            .ok_or(ParseError::BadIndex)?;
        ReadScope::new(data).read::<FontDict>()
    }
}

impl TryFrom<u16> for Operator {
    type Error = ParseError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        if (value & 0xFF00) == (12 << 8) {
            match value as u8 {
                0 => Ok(Operator::Copyright),
                1 => Ok(Operator::IsFixedPitch),
                2 => Ok(Operator::ItalicAngle),
                3 => Ok(Operator::UnderlinePosition),
                4 => Ok(Operator::UnderlineThickness),
                5 => Ok(Operator::PaintType),
                6 => Ok(Operator::CharstringType),
                7 => Ok(Operator::FontMatrix),
                8 => Ok(Operator::StrokeWidth),
                9 => Ok(Operator::BlueScale),
                10 => Ok(Operator::BlueShift),
                11 => Ok(Operator::BlueFuzz),
                12 => Ok(Operator::StemSnapH),
                13 => Ok(Operator::StemSnapV),
                14 => Ok(Operator::ForceBold),
                17 => Ok(Operator::LanguageGroup),
                18 => Ok(Operator::ExpansionFactor),
                19 => Ok(Operator::InitialRandomSeed),
                20 => Ok(Operator::SyntheticBase),
                21 => Ok(Operator::PostScript),
                22 => Ok(Operator::BaseFontName),
                23 => Ok(Operator::BaseFontBlend),
                30 => Ok(Operator::ROS),
                31 => Ok(Operator::CIDFontVersion),
                32 => Ok(Operator::CIDFontRevision),
                33 => Ok(Operator::CIDFontType),
                34 => Ok(Operator::CIDCount),
                35 => Ok(Operator::UIDBase),
                36 => Ok(Operator::FDArray),
                37 => Ok(Operator::FDSelect),
                38 => Ok(Operator::FontName),
                _ => Err(ParseError::BadValue),
            }
        } else {
            match value {
                0 => Ok(Operator::Version),
                1 => Ok(Operator::Notice),
                2 => Ok(Operator::FullName),
                3 => Ok(Operator::FamilyName),
                4 => Ok(Operator::Weight),
                5 => Ok(Operator::FontBBox),
                6 => Ok(Operator::BlueValues),
                7 => Ok(Operator::OtherBlues),
                8 => Ok(Operator::FamilyBlues),
                9 => Ok(Operator::FamilyOtherBlues),
                10 => Ok(Operator::StdHW),
                11 => Ok(Operator::StdVW),
                13 => Ok(Operator::UniqueID),
                14 => Ok(Operator::XUID),
                15 => Ok(Operator::Charset),
                16 => Ok(Operator::Encoding),
                17 => Ok(Operator::CharStrings),
                18 => Ok(Operator::Private),
                19 => Ok(Operator::Subrs),
                20 => Ok(Operator::DefaultWidthX),
                21 => Ok(Operator::NominalWidthX),
                _ => Err(ParseError::BadValue),
            }
        }
    }
}

impl Operand {
    pub fn is_offset(&self) -> bool {
        match self {
            Operand::Offset(_) => true,
            _ => false,
        }
    }
}

impl<'a> Font<'a> {
    pub fn is_cid_keyed(&self) -> bool {
        match self.data {
            CFFVariant::CID(_) => true,
            CFFVariant::Type1(_) => false,
        }
    }
}

fn lookup_offset_index(off_size: u8, offset_array: &[u8], index: usize) -> usize {
    let buf = &offset_array[index * usize::from(off_size)..];
    match off_size {
        1 => buf[0] as usize,
        2 => BigEndian::read_u16(buf) as usize,
        3 => BigEndian::read_u24(buf) as usize,
        4 => BigEndian::read_u32(buf) as usize,
        _ => panic!("unexpected off_size"),
    }
}

fn read_range_array<'a, F, N>(
    ctxt: &mut ReadCtxt<'a>,
    n_glyphs: usize,
) -> Result<ReadArray<'a, Range<F, N>>, ParseError>
where
    Range<F, N>: ReadFrom<'a>,
    usize: From<N>,
    N: num::Unsigned + Copy,
{
    let mut peek = ctxt.scope().ctxt();
    let mut range_count = 0;
    let mut glyphs_covered = 0;
    while glyphs_covered < n_glyphs {
        let range = peek.read::<Range<F, N>>()?;
        range_count += 1;
        glyphs_covered += range.len();
    }

    ctxt.read_array::<Range<F, N>>(range_count)
}

fn write_cff_variant<'a, C: WriteContext>(
    ctxt: &mut C,
    variant: &CFFVariant<'a>,
    top_dict_delta: &mut DictDelta,
) -> Result<(), WriteError> {
    match variant {
        CFFVariant::CID(cid_data) => {
            let offsets = CIDData::write(ctxt, &cid_data)?;
            top_dict_delta.push_offset(Operator::FDArray, i32::try_from(offsets.font_dict_index)?);
            top_dict_delta.push_offset(Operator::FDSelect, i32::try_from(offsets.fd_select)?);
        }
        CFFVariant::Type1(type1_data) => {
            let offsets = Type1Data::write(ctxt, &type1_data)?;
            if let Some(custom_encoding_offset) = offsets.custom_encoding {
                top_dict_delta
                    .push_offset(Operator::Encoding, i32::try_from(custom_encoding_offset)?);
            }
            top_dict_delta.push(
                Operator::Private,
                vec![
                    Operand::Offset(i32::try_from(offsets.private_dict_len)?),
                    Operand::Offset(i32::try_from(offsets.private_dict)?),
                ],
            );
        }
    }

    Ok(())
}

// NOTE: Ideally the following read_* functions would ReadBinary or ReadBinaryDep impls.
// However we need to be able to indicate that the borrowed TopDict has a different lifetime
// to the other aspects, which is not currently possible.
// This post sums up the issue fairly well:
// https://lukaskalbertodt.github.io/2018/08/03/solving-the-generalized-streaming-iterator-problem-without-gats.html
// Rust tracking issue: https://github.com/rust-lang/rust/issues/44265

fn read_cid_data<'a>(
    scope: &ReadScope<'a>,
    top_dict: &TopDict,
    n_glyphs: usize,
) -> Result<CIDData<'a>, ParseError> {
    // The Top DICT begins with ROS operator
    // which specifies the Registry-Ordering-Supplement for the font.
    // This will indicate to a CFF parser that special CID processing
    // should be applied to this font. Specifically:
    //
    // • The FDArray operator is expected to be present, with a single
    //   argument specifying an offset to the Font DICT INDEX. Each
    //   Font DICT in this array specifies information unique to a
    //   particular group of glyphs in the font.
    let offset = top_dict
        .get_i32(Operator::FDArray)
        .ok_or(ParseError::MissingValue)??;
    let font_dict_index = scope.offset(usize::try_from(offset)?).read::<Index<'a>>()?;

    let offset = top_dict
        .get_i32(Operator::FDSelect)
        .ok_or(ParseError::MissingValue)??;
    let fd_select = scope
        .offset(usize::try_from(offset)?)
        .read_dep::<FDSelect<'a>>(n_glyphs)?;

    let mut private_dicts = Vec::with_capacity(font_dict_index.count);
    let mut local_subr_indices = Vec::with_capacity(font_dict_index.count);
    for object in font_dict_index.iter() {
        let font_dict = ReadScope::new(object).read::<FontDict>()?;
        let (private_dict, private_dict_offset) = font_dict.read_private_dict(scope)?;
        let local_subr_index = read_local_subr_index(scope, &private_dict, private_dict_offset)?;

        private_dicts.push(private_dict);
        local_subr_indices.push(local_subr_index);
    }

    Ok(CIDData {
        font_dict_index: MaybeOwnedIndex::Borrowed(font_dict_index),
        private_dicts,
        local_subr_indices,
        fd_select,
    })
}

impl<'a> WriteBinary<&Self> for CIDData<'a> {
    type Output = CIDDataOffsets;

    fn write<C: WriteContext>(ctxt: &mut C, data: &Self) -> Result<Self::Output, WriteError> {
        // Private DICTs and Local subroutines
        let mut private_dict_offset_lengths = Vec::with_capacity(data.private_dicts.len());
        for (private_dict, local_subr_index) in data
            .private_dicts
            .iter()
            .zip(data.local_subr_indices.iter())
        {
            let offset = ctxt.bytes_written();
            let written_length =
                write_private_dict_and_local_subr_index(ctxt, private_dict, local_subr_index)?;
            private_dict_offset_lengths.push((offset, written_length));
        }

        // Font DICT INDEX
        let mut font_dict_data = WriteBuffer::new();
        let mut font_dict_offsets = Vec::with_capacity(data.font_dict_index.len());
        for (object, (offset, length)) in data
            .font_dict_index
            .iter()
            .zip(private_dict_offset_lengths.into_iter())
        {
            let font_dict = ReadScope::new(object)
                .read::<FontDict>()
                .map_err(|_err| WriteError::BadValue)?;
            let mut font_dict_delta = DictDelta::new();
            font_dict_delta.push(
                Operator::Private,
                vec![
                    Operand::Offset(i32::try_from(length)?),
                    Operand::Offset(i32::try_from(offset)?),
                ],
            );

            font_dict_offsets.push(font_dict_data.bytes_written() + 1); // +1 INDEXes start at offset 1
            FontDict::write_dep(&mut font_dict_data, &font_dict, font_dict_delta)?;
        }
        let last_font_dict_offset = font_dict_data.bytes_written() + 1;
        font_dict_offsets.push(last_font_dict_offset);

        let (off_size, offset_array) = serialise_offset_array(font_dict_offsets)?;
        let font_dict_index = Index {
            count: data.font_dict_index.len(),
            off_size,
            offset_array: &offset_array,
            data_array: font_dict_data.bytes(),
        };
        let font_dict_index_offset = ctxt.bytes_written();
        Index::write(ctxt, &font_dict_index)?;

        let fd_select_offset = ctxt.bytes_written();
        FDSelect::write(ctxt, &data.fd_select)?;

        Ok(CIDDataOffsets {
            font_dict_index: font_dict_index_offset,
            fd_select: fd_select_offset,
        })
    }
}

impl<'a> WriteBinary<&Self> for Type1Data<'a> {
    type Output = Type1DataOffsets;

    fn write<C: WriteContext>(ctxt: &mut C, data: &Self) -> Result<Self::Output, WriteError> {
        let mut offsets = Type1DataOffsets {
            custom_encoding: None,
            private_dict: ctxt.bytes_written(),
            private_dict_len: 0,
        };

        offsets.private_dict_len = write_private_dict_and_local_subr_index(
            ctxt,
            &data.private_dict,
            &data.local_subr_index,
        )?;

        if let Type1Data {
            encoding: Encoding::Custom(ref custom_encoding),
            ..
        } = data
        {
            offsets.custom_encoding = Some(ctxt.bytes_written());
            CustomEncoding::write(ctxt, custom_encoding)?;
        }

        Ok(offsets)
    }
}

/// Write the Private DICT and local subrs if present, returns the length of the Private DICT
fn write_private_dict_and_local_subr_index<'a, C: WriteContext>(
    ctxt: &mut C,
    private_dict: &PrivateDict,
    local_subr_index: &Option<Index<'a>>,
) -> Result<usize, WriteError> {
    // Determine how big the Private DICT will be
    let private_dict_length =
        PrivateDict::write_dep(&mut WriteCounter::new(), private_dict, DictDelta::new())?;

    // Write Private DICT with updated offset to Local subroutines if present
    let mut private_dict_delta = DictDelta::new();
    if local_subr_index.is_some() {
        // This offset is relative to the start of the Private DICT
        private_dict_delta.push_offset(Operator::Subrs, i32::try_from(private_dict_length)?);
    }
    let written_length = PrivateDict::write_dep(ctxt, &private_dict, private_dict_delta)?;
    assert_eq!(written_length, private_dict_length);

    if let Some(local_subr_index) = local_subr_index {
        Index::write(ctxt, local_subr_index)?;
    }

    Ok(written_length)
}

// The CFF spec notes:
// A few fonts have multiply-encoded glyphs which are not supported directly by any of the above
// formats. This situation is indicated by setting the high-order bit in the format byte and
// supplementing the encoding.
//
// This is not handed as it is not expected that these will be encountered in CFF in OTF files.
fn read_encoding<'a>(
    scope: &ReadScope<'a>,
    top_dict: &TopDict,
) -> Result<Encoding<'a>, ParseError> {
    let offset = top_dict
        .get_i32(Operator::Encoding)
        .ok_or(ParseError::MissingValue)??;
    let encoding = match offset {
        0 => Encoding::Standard,
        1 => Encoding::Expert,
        _ => Encoding::Custom(
            scope
                .offset(usize::try_from(offset)?)
                .read::<CustomEncoding<'_>>()?,
        ),
    };

    Ok(encoding)
}

fn read_charset<'a>(
    scope: &ReadScope<'a>,
    top_dict: &TopDict,
    char_strings_count: usize,
) -> Result<Charset<'a>, ParseError> {
    let offset = top_dict
        .get_i32(Operator::Charset)
        .ok_or(ParseError::MissingValue)??;
    let charset = match offset {
        0 => Charset::ISOAdobe,
        1 => Charset::Expert,
        2 => Charset::ExpertSubset,
        _ => Charset::Custom(
            scope
                .offset(usize::try_from(offset)?)
                .read_dep::<CustomCharset<'_>>(char_strings_count)?,
        ),
    };

    Ok(charset)
}

fn read_local_subr_index<'a>(
    scope: &ReadScope<'a>,
    private_dict: &PrivateDict,
    private_dict_offset: usize,
) -> Result<Option<Index<'a>>, ParseError> {
    // Local subrs are stored in an INDEX structure which is located via the offset operand
    // of the Subrs operator in the Private DICT. A font without local subrs has no Subrs
    // operator in the Private DICT. The local subrs offset is relative to the beginning of
    // the Private DICT data.
    private_dict
        .get_i32(Operator::Subrs)
        .transpose()?
        .map(|offset| {
            let offset = usize::try_from(offset)?;
            scope
                .offset(private_dict_offset + offset)
                .read::<Index<'_>>()
        })
        .transpose()
}

/// Serialise the offsets using an optimal `off_size`, returning that and the serialised data.
fn serialise_offset_array(offsets: Vec<usize>) -> Result<(u8, Vec<u8>), WriteError> {
    if offsets.is_empty() {
        return Ok((1, Vec::new()));
    }

    // NOTE(unwrap): Safe due to is_empty check
    let off_size = offset_size(*offsets.last().unwrap()).ok_or(WriteError::BadValue)?;
    let mut offset_array = WriteBuffer::new();
    match off_size {
        1 => offset_array
            .write_vec::<U8>(offsets.into_iter().map(|offset| offset as u8).collect())?,

        2 => offset_array
            .write_vec::<U16Be>(offsets.into_iter().map(|offset| offset as u16).collect())?,

        3 => offset_array
            .write_vec::<U24Be>(offsets.into_iter().map(|offset| offset as u32).collect())?,

        4 => offset_array
            .write_vec::<U32Be>(offsets.into_iter().map(|offset| offset as u32).collect())?,

        _ => unreachable!(), // offset_size only returns 1..=4
    }

    Ok((off_size, offset_array.into_inner()))
}

const STANDARD_STRINGS: [&str; 391] = [
    ".notdef",
    "space",
    "exclam",
    "quotedbl",
    "numbersign",
    "dollar",
    "percent",
    "ampersand",
    "quoteright",
    "parenleft",
    "parenright",
    "asterisk",
    "plus",
    "comma",
    "hyphen",
    "period",
    "slash",
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "colon",
    "semicolon",
    "less",
    "equal",
    "greater",
    "question",
    "at",
    "A",
    "B",
    "C",
    "D",
    "E",
    "F",
    "G",
    "H",
    "I",
    "J",
    "K",
    "L",
    "M",
    "N",
    "O",
    "P",
    "Q",
    "R",
    "S",
    "T",
    "U",
    "V",
    "W",
    "X",
    "Y",
    "Z",
    "bracketleft",
    "backslash",
    "bracketright",
    "asciicircum",
    "underscore",
    "quoteleft",
    "a",
    "b",
    "c",
    "d",
    "e",
    "f",
    "g",
    "h",
    "i",
    "j",
    "k",
    "l",
    "m",
    "n",
    "o",
    "p",
    "q",
    "r",
    "s",
    "t",
    "u",
    "v",
    "w",
    "x",
    "y",
    "z",
    "braceleft",
    "bar",
    "braceright",
    "asciitilde",
    "exclamdown",
    "cent",
    "sterling",
    "fraction",
    "yen",
    "florin",
    "section",
    "currency",
    "quotesingle",
    "quotedblleft",
    "guillemotleft",
    "guilsinglleft",
    "guilsinglright",
    "fi",
    "fl",
    "endash",
    "dagger",
    "daggerdbl",
    "periodcentered",
    "paragraph",
    "bullet",
    "quotesinglbase",
    "quotedblbase",
    "quotedblright",
    "guillemotright",
    "ellipsis",
    "perthousand",
    "questiondown",
    "grave",
    "acute",
    "circumflex",
    "tilde",
    "macron",
    "breve",
    "dotaccent",
    "dieresis",
    "ring",
    "cedilla",
    "hungarumlaut",
    "ogonek",
    "caron",
    "emdash",
    "AE",
    "ordfeminine",
    "Lslash",
    "Oslash",
    "OE",
    "ordmasculine",
    "ae",
    "dotlessi",
    "lslash",
    "oslash",
    "oe",
    "germandbls",
    "onesuperior",
    "logicalnot",
    "mu",
    "trademark",
    "Eth",
    "onehalf",
    "plusminus",
    "Thorn",
    "onequarter",
    "divide",
    "brokenbar",
    "degree",
    "thorn",
    "threequarters",
    "twosuperior",
    "registered",
    "minus",
    "eth",
    "multiply",
    "threesuperior",
    "copyright",
    "Aacute",
    "Acircumflex",
    "Adieresis",
    "Agrave",
    "Aring",
    "Atilde",
    "Ccedilla",
    "Eacute",
    "Ecircumflex",
    "Edieresis",
    "Egrave",
    "Iacute",
    "Icircumflex",
    "Idieresis",
    "Igrave",
    "Ntilde",
    "Oacute",
    "Ocircumflex",
    "Odieresis",
    "Ograve",
    "Otilde",
    "Scaron",
    "Uacute",
    "Ucircumflex",
    "Udieresis",
    "Ugrave",
    "Yacute",
    "Ydieresis",
    "Zcaron",
    "aacute",
    "acircumflex",
    "adieresis",
    "agrave",
    "aring",
    "atilde",
    "ccedilla",
    "eacute",
    "ecircumflex",
    "edieresis",
    "egrave",
    "iacute",
    "icircumflex",
    "idieresis",
    "igrave",
    "ntilde",
    "oacute",
    "ocircumflex",
    "odieresis",
    "ograve",
    "otilde",
    "scaron",
    "uacute",
    "ucircumflex",
    "udieresis",
    "ugrave",
    "yacute",
    "ydieresis",
    "zcaron",
    "exclamsmall",
    "Hungarumlautsmall",
    "dollaroldstyle",
    "dollarsuperior",
    "ampersandsmall",
    "Acutesmall",
    "parenleftsuperior",
    "parenrightsuperior",
    "twodotenleader",
    "onedotenleader",
    "zerooldstyle",
    "oneoldstyle",
    "twooldstyle",
    "threeoldstyle",
    "fouroldstyle",
    "fiveoldstyle",
    "sixoldstyle",
    "sevenoldstyle",
    "eightoldstyle",
    "nineoldstyle",
    "commasuperior",
    "threequartersemdash",
    "periodsuperior",
    "questionsmall",
    "asuperior",
    "bsuperior",
    "centsuperior",
    "dsuperior",
    "esuperior",
    "isuperior",
    "lsuperior",
    "msuperior",
    "nsuperior",
    "osuperior",
    "rsuperior",
    "ssuperior",
    "tsuperior",
    "ff",
    "ffi",
    "ffl",
    "parenleftinferior",
    "parenrightinferior",
    "Circumflexsmall",
    "hyphensuperior",
    "Gravesmall",
    "Asmall",
    "Bsmall",
    "Csmall",
    "Dsmall",
    "Esmall",
    "Fsmall",
    "Gsmall",
    "Hsmall",
    "Ismall",
    "Jsmall",
    "Ksmall",
    "Lsmall",
    "Msmall",
    "Nsmall",
    "Osmall",
    "Psmall",
    "Qsmall",
    "Rsmall",
    "Ssmall",
    "Tsmall",
    "Usmall",
    "Vsmall",
    "Wsmall",
    "Xsmall",
    "Ysmall",
    "Zsmall",
    "colonmonetary",
    "onefitted",
    "rupiah",
    "Tildesmall",
    "exclamdownsmall",
    "centoldstyle",
    "Lslashsmall",
    "Scaronsmall",
    "Zcaronsmall",
    "Dieresissmall",
    "Brevesmall",
    "Caronsmall",
    "Dotaccentsmall",
    "Macronsmall",
    "figuredash",
    "hypheninferior",
    "Ogoneksmall",
    "Ringsmall",
    "Cedillasmall",
    "questiondownsmall",
    "oneeighth",
    "threeeighths",
    "fiveeighths",
    "seveneighths",
    "onethird",
    "twothirds",
    "zerosuperior",
    "foursuperior",
    "fivesuperior",
    "sixsuperior",
    "sevensuperior",
    "eightsuperior",
    "ninesuperior",
    "zeroinferior",
    "oneinferior",
    "twoinferior",
    "threeinferior",
    "fourinferior",
    "fiveinferior",
    "sixinferior",
    "seveninferior",
    "eightinferior",
    "nineinferior",
    "centinferior",
    "dollarinferior",
    "periodinferior",
    "commainferior",
    "Agravesmall",
    "Aacutesmall",
    "Acircumflexsmall",
    "Atildesmall",
    "Adieresissmall",
    "Aringsmall",
    "AEsmall",
    "Ccedillasmall",
    "Egravesmall",
    "Eacutesmall",
    "Ecircumflexsmall",
    "Edieresissmall",
    "Igravesmall",
    "Iacutesmall",
    "Icircumflexsmall",
    "Idieresissmall",
    "Ethsmall",
    "Ntildesmall",
    "Ogravesmall",
    "Oacutesmall",
    "Ocircumflexsmall",
    "Otildesmall",
    "Odieresissmall",
    "OEsmall",
    "Oslashsmall",
    "Ugravesmall",
    "Uacutesmall",
    "Ucircumflexsmall",
    "Udieresissmall",
    "Yacutesmall",
    "Thornsmall",
    "Ydieresissmall",
    "001.000",
    "001.001",
    "001.002",
    "001.003",
    "Black",
    "Bold",
    "Book",
    "Light",
    "Medium",
    "Regular",
    "Roman",
    "Semibold",
];

const EXPERT_CHARSET: [u16; 166] = [
    0,   // .notdef
    1,   // space
    229, // exclamsmall
    230, // Hungarumlautsmall
    231, // dollaroldstyle
    232, // dollarsuperior
    233, // ampersandsmall
    234, // Acutesmall
    235, // parenleftsuperior
    236, // parenrightsuperior
    237, // twodotenleader
    238, // onedotenleader
    13,  // comma
    14,  // hyphen
    15,  // period
    99,  // fraction
    239, // zerooldstyle
    240, // oneoldstyle
    241, // twooldstyle
    242, // threeoldstyle
    243, // fouroldstyle
    244, // fiveoldstyle
    245, // sixoldstyle
    246, // sevenoldstyle
    247, // eightoldstyle
    248, // nineoldstyle
    27,  // colon
    28,  // semicolon
    249, // commasuperior
    250, // threequartersemdash
    251, // periodsuperior
    252, // questionsmall
    253, // asuperior
    254, // bsuperior
    255, // centsuperior
    256, // dsuperior
    257, // esuperior
    258, // isuperior
    259, // lsuperior
    260, // msuperior
    261, // nsuperior
    262, // osuperior
    263, // rsuperior
    264, // ssuperior
    265, // tsuperior
    266, // ff
    109, // fi
    110, // fl
    267, // ffi
    268, // ffl
    269, // parenleftinferior
    270, // parenrightinferior
    271, // Circumflexsmall
    272, // hyphensuperior
    273, // Gravesmall
    274, // Asmall
    275, // Bsmall
    276, // Csmall
    277, // Dsmall
    278, // Esmall
    279, // Fsmall
    280, // Gsmall
    281, // Hsmall
    282, // Ismall
    283, // Jsmall
    284, // Ksmall
    285, // Lsmall
    286, // Msmall
    287, // Nsmall
    288, // Osmall
    289, // Psmall
    290, // Qsmall
    291, // Rsmall
    292, // Ssmall
    293, // Tsmall
    294, // Usmall
    295, // Vsmall
    296, // Wsmall
    297, // Xsmall
    298, // Ysmall
    299, // Zsmall
    300, // colonmonetary
    301, // onefitted
    302, // rupiah
    303, // Tildesmall
    304, // exclamdownsmall
    305, // centoldstyle
    306, // Lslashsmall
    307, // Scaronsmall
    308, // Zcaronsmall
    309, // Dieresissmall
    310, // Brevesmall
    311, // Caronsmall
    312, // Dotaccentsmall
    313, // Macronsmall
    314, // figuredash
    315, // hypheninferior
    316, // Ogoneksmall
    317, // Ringsmall
    318, // Cedillasmall
    158, // onequarter
    155, // onehalf
    163, // threequarters
    319, // questiondownsmall
    320, // oneeighth
    321, // threeeighths
    322, // fiveeighths
    323, // seveneighths
    324, // onethird
    325, // twothirds
    326, // zerosuperior
    150, // onesuperior
    164, // twosuperior
    169, // threesuperior
    327, // foursuperior
    328, // fivesuperior
    329, // sixsuperior
    330, // sevensuperior
    331, // eightsuperior
    332, // ninesuperior
    333, // zeroinferior
    334, // oneinferior
    335, // twoinferior
    336, // threeinferior
    337, // fourinferior
    338, // fiveinferior
    339, // sixinferior
    340, // seveninferior
    341, // eightinferior
    342, // nineinferior
    343, // centinferior
    344, // dollarinferior
    345, // periodinferior
    346, // commainferior
    347, // Agravesmall
    348, // Aacutesmall
    349, // Acircumflexsmall
    350, // Atildesmall
    351, // Adieresissmall
    352, // Aringsmall
    353, // AEsmall
    354, // Ccedillasmall
    355, // Egravesmall
    356, // Eacutesmall
    357, // Ecircumflexsmall
    358, // Edieresissmall
    359, // Igravesmall
    360, // Iacutesmall
    361, // Icircumflexsmall
    362, // Idieresissmall
    363, // Ethsmall
    364, // Ntildesmall
    365, // Ogravesmall
    366, // Oacutesmall
    367, // Ocircumflexsmall
    368, // Otildesmall
    369, // Odieresissmall
    370, // OEsmall
    371, // Oslashsmall
    372, // Ugravesmall
    373, // Uacutesmall
    374, // Ucircumflexsmall
    375, // Udieresissmall
    376, // Yacutesmall
    377, // Thornsmall
    378, // Ydieresissmall
];

const EXPERT_SUBSET_CHARSET: [u16; 87] = [
    0,   // .notdef
    1,   // space
    231, // dollaroldstyle
    232, // dollarsuperior
    235, // parenleftsuperior
    236, // parenrightsuperior
    237, // twodotenleader
    238, // onedotenleader
    13,  // comma
    14,  // hyphen
    15,  // period
    99,  // fraction
    239, // zerooldstyle
    240, // oneoldstyle
    241, // twooldstyle
    242, // threeoldstyle
    243, // fouroldstyle
    244, // fiveoldstyle
    245, // sixoldstyle
    246, // sevenoldstyle
    247, // eightoldstyle
    248, // nineoldstyle
    27,  // colon
    28,  // semicolon
    249, // commasuperior
    250, // threequartersemdash
    251, // periodsuperior
    253, // asuperior
    254, // bsuperior
    255, // centsuperior
    256, // dsuperior
    257, // esuperior
    258, // isuperior
    259, // lsuperior
    260, // msuperior
    261, // nsuperior
    262, // osuperior
    263, // rsuperior
    264, // ssuperior
    265, // tsuperior
    266, // ff
    109, // fi
    110, // fl
    267, // ffi
    268, // ffl
    269, // parenleftinferior
    270, // parenrightinferior
    272, // hyphensuperior
    300, // colonmonetary
    301, // onefitted
    302, // rupiah
    305, // centoldstyle
    314, // figuredash
    315, // hypheninferior
    158, // onequarter
    155, // onehalf
    163, // threequarters
    320, // oneeighth
    321, // threeeighths
    322, // fiveeighths
    323, // seveneighths
    324, // onethird
    325, // twothirds
    326, // zerosuperior
    150, // onesuperior
    164, // twosuperior
    169, // threesuperior
    327, // foursuperior
    328, // fivesuperior
    329, // sixsuperior
    330, // sevensuperior
    331, // eightsuperior
    332, // ninesuperior
    333, // zeroinferior
    334, // oneinferior
    335, // twoinferior
    336, // threeinferior
    337, // fourinferior
    338, // fiveinferior
    339, // sixinferior
    340, // seveninferior
    341, // eightinferior
    342, // nineinferior
    343, // centinferior
    344, // dollarinferior
    345, // periodinferior
    346, // commainferior
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binary::read::ReadScope;

    #[test]
    fn test_iter_index() {
        let offset_array = [1, 2, 3];
        let data_array = [4, 5];
        let index = Index {
            count: 2,
            off_size: 1,
            offset_array: &offset_array,
            data_array: &data_array,
        };

        assert_eq!(index.iter().collect::<Vec<_>>(), vec![[4], [5]]);
    }

    #[test]
    fn test_read_op1() {
        let mut ctxt = ReadScope::new(&[0, 0]).ctxt();
        assert_eq!(
            Op::read(&mut ctxt).unwrap(),
            Op::Operator(Operator::Version)
        );
    }

    #[test]
    fn test_fail_op1() {
        let mut ctxt = ReadScope::new(&[]).ctxt();
        assert!(Op::read(&mut ctxt).is_err());
    }

    #[test]
    fn test_read_op2() {
        let mut ctxt = ReadScope::new(&[12, 1]).ctxt();
        assert_eq!(
            Op::read(&mut ctxt).unwrap(),
            Op::Operator(Operator::IsFixedPitch)
        );
    }

    #[test]
    fn test_fail_op2() {
        let mut ctxt = ReadScope::new(&[12]).ctxt();
        assert!(Op::read(&mut ctxt).is_err());
    }

    #[test]
    fn test_read_i8() {
        let mut ctxt = ReadScope::new(&[0x8b]).ctxt();
        assert_eq!(
            Op::read(&mut ctxt).unwrap(),
            Op::Operand(Operand::Integer(0))
        );
    }

    #[test]
    fn test_read_i16() {
        //                             _____-10000______  ______10000_____  100   -100
        let mut ctxt = ReadScope::new(&[0x1c, 0xd8, 0xf0, 0x1c, 0x27, 0x10, 0xef, 0x27]).ctxt();
        assert_eq!(
            Op::read(&mut ctxt).unwrap(),
            Op::Operand(Operand::Integer(-10000))
        );
        assert_eq!(
            Op::read(&mut ctxt).unwrap(),
            Op::Operand(Operand::Integer(10000))
        );
        assert_eq!(
            Op::read(&mut ctxt).unwrap(),
            Op::Operand(Operand::Integer(100))
        );
        assert_eq!(
            Op::read(&mut ctxt).unwrap(),
            Op::Operand(Operand::Integer(-100))
        );
    }

    #[test]
    fn test_read_i32() {
        //                   __________-100000___________  ____________100000__________
        let mut ctxt =
            ReadScope::new(&[0x1d, 0xff, 0xfe, 0x79, 0x60, 0x1d, 0x00, 0x01, 0x86, 0xa0]).ctxt();
        assert_eq!(
            Op::read(&mut ctxt).unwrap(),
            Op::Operand(Operand::Integer(-100000))
        );
        assert_eq!(
            Op::read(&mut ctxt).unwrap(),
            Op::Operand(Operand::Integer(100000))
        );
    }

    #[test]
    fn test_read_real() {
        let mut ctxt = ReadScope::new(&[
            // ______-2.25________  _______________0.140541E–3______________
            0x1e, 0xe2, 0xa2, 0x5f, 0x1e, 0x0a, 0x14, 0x05, 0x41, 0xc3, 0xff,
        ])
        .ctxt();
        assert_eq!(
            Op::read(&mut ctxt).unwrap(),
            Op::Operand(Operand::Real(Real(vec![0xe2, 0xa2, 0x5f])))
        );
        assert_eq!(
            Op::read(&mut ctxt).unwrap(),
            Op::Operand(Operand::Real(Real(vec![
                0x0a, 0x14, 0x05, 0x41, 0xc3, 0xff
            ])))
        );
    }

    #[test]
    fn test_read_top_dict() {
        let expected = TopDict {
            dict: vec![
                (Operator::IsFixedPitch, vec![Operand::Integer(1)]),
                (Operator::Notice, vec![Operand::Integer(123)]),
            ],
            default: PhantomData,
        };
        // IsFixedPitch (12 1) is true (1)
        // Notice (1) SID is 123
        //                              _1__         __123__
        let mut ctxt = ReadScope::new(&[0x8c, 12, 1, 247, 15, 1]).ctxt();
        assert_eq!(TopDict::read(&mut ctxt).unwrap(), expected);
    }

    #[test]
    fn test_write_top_dict() {
        let dict = TopDict {
            dict: vec![
                (Operator::IsFixedPitch, vec![Operand::Integer(1)]),
                (Operator::Notice, vec![Operand::Integer(123)]),
                // This one is omitted in the output because it is the default for this operator
                (Operator::PaintType, vec![Operand::Integer(0)]),
            ],
            default: PhantomData,
        };
        // IsFixedPitch Op2(1) is true (1)
        // Notice Op1(1) SID is 123
        //              _1__  Op2(1) __123__  Op1(1)
        let expected = [0x8c, 12, 1, 247, 15, 1];
        let mut ctxt = WriteBuffer::new();
        TopDict::write_dep(&mut ctxt, &dict, DictDelta::new()).unwrap();

        assert_eq!(ctxt.bytes(), &expected);
    }

    #[test]
    fn test_write_top_dict_delta() {
        let dict = TopDict {
            dict: vec![(Operator::CharStrings, vec![Operand::Offset(123)])],
            default: PhantomData,
        };
        let mut delta = DictDelta::new();
        delta.push(Operator::CharStrings, vec![Operand::Offset(1000)]);

        // CharStrings is operator 17 and takes an offset (number as operand)
        // Offsets are always written out using the 5 byte representation
        //              _______1000_________
        let expected = [29, 0, 0, 0x03, 0xE8, 17];
        let mut ctxt = WriteBuffer::new();
        TopDict::write_dep(&mut ctxt, &dict, delta).unwrap();

        // It's expected that the delta value is used instead of the dict value
        assert_eq!(ctxt.bytes(), &expected);
    }

    #[test]
    fn test_write_top_dict_size() {
        let dict = TopDict {
            dict: vec![
                (Operator::IsFixedPitch, vec![Operand::Integer(1)]),
                (Operator::Notice, vec![Operand::Integer(123)]),
                // This one is omitted in the output because it is the default for this operator
                (Operator::PaintType, vec![Operand::Integer(0)]),
            ],
            default: PhantomData,
        };
        let mut counter = WriteCounter::new();
        TopDict::write_dep(&mut counter, &dict, DictDelta::new()).unwrap();

        assert_eq!(counter.bytes_written(), 6);
    }

    #[test]
    fn test_read_top_dict_operand_limit() {
        let mut ctxt = ReadScope::new(&[0x8c; MAX_OPERANDS + 1]).ctxt();
        match TopDict::read(&mut ctxt) {
            Err(ParseError::LimitExceeded) => {}
            _ => panic!("expected Err(ParseError::LimitExceeded) got something else"),
        }
    }

    #[test]
    fn test_read_empty_private_dict() {
        // A Private DICT is required, but may be specified as having a length of 0 if there are
        // no non-default values to be stored.
        let dict = ReadScope::new(&[]).read::<PrivateDict>();
        assert!(dict.is_ok());
    }

    #[test]
    fn test_read_custom_encoding_format0() {
        let data_format0 = [0, 3, 4, 5, 6];
        let mut ctxt = ReadScope::new(&data_format0).ctxt();
        let format0_encoding = ctxt.read::<CustomEncoding<'_>>().unwrap();
        match format0_encoding {
            CustomEncoding::Format0 { codes } => {
                assert_eq!(codes.iter().collect_vec(), vec![4, 5, 6])
            }
            _ => panic!("expected CustomEncoding::Format0 got something else"),
        }
    }

    #[test]
    fn test_read_custom_encoding_format1() {
        let data_format1 = [1, 2, 4, 5, 6, 7];
        let mut ctxt = ReadScope::new(&data_format1).ctxt();
        let format1_encoding = ctxt.read::<CustomEncoding<'_>>().unwrap();
        match format1_encoding {
            CustomEncoding::Format1 { ranges } => assert_eq!(
                ranges.iter().collect_vec(),
                vec![
                    Range {
                        first: 4,
                        n_left: 5
                    },
                    Range {
                        first: 6,
                        n_left: 7
                    }
                ]
            ),
            _ => panic!("expected CustomEncoding::Format1 got something else"),
        }
    }

    #[test]
    fn test_read_custom_charset_format0() {
        let n_glyphs = 2;
        let data_format0 = [0, 0xAA, 0xBB];
        let mut ctxt = ReadScope::new(&data_format0).ctxt();
        let format0_charset = ctxt.read_dep::<CustomCharset<'_>>(n_glyphs).unwrap();
        match format0_charset {
            CustomCharset::Format0 { glyphs } => {
                assert_eq!(glyphs.iter().collect_vec(), vec![0xAABB])
            }
            _ => panic!("expected CustomCharset::Format0 got something else"),
        }
    }

    #[test]
    fn test_read_custom_charset_format1() {
        let n_glyphs = 5;
        let data_format1 = [1, 0, 1, 3];
        let mut ctxt = ReadScope::new(&data_format1).ctxt();
        let format1_charset = ctxt.read_dep::<CustomCharset<'_>>(n_glyphs).unwrap();
        match format1_charset {
            CustomCharset::Format1 { ranges } => assert_eq!(
                ranges.iter().collect_vec(),
                vec![Range {
                    first: 1,
                    n_left: 3
                },]
            ),
            _ => panic!("expected CustomCharset::Format1 got something else"),
        }
    }

    #[test]
    fn test_read_custom_charset_format2() {
        let n_glyphs = 5;
        let data_format2 = [2, 0, 1, 0, 3];
        let mut ctxt = ReadScope::new(&data_format2).ctxt();
        let format2_charset = ctxt.read_dep::<CustomCharset<'_>>(n_glyphs).unwrap();
        match format2_charset {
            CustomCharset::Format2 { ranges } => assert_eq!(
                ranges.iter().collect_vec(),
                vec![Range {
                    first: 1,
                    n_left: 3
                },]
            ),
            _ => panic!("expected CustomCharset::Format2 got something else"),
        }
    }

    #[test]
    fn test_read_write_index() {
        let mut count = vec![0, 1];
        let off_size = 3;
        let mut offset0 = vec![0, 0, 1];
        let mut offset1 = vec![0, 0, 2];
        let object = 5;

        // The data is built up like so it's easier to see what each value represents
        let mut data = Vec::new();
        data.append(&mut count);
        data.push(off_size);
        data.append(&mut offset0);
        data.append(&mut offset1);
        data.push(object);

        // Read
        let mut ctxt = ReadScope::new(&data).ctxt();
        let index = ctxt.read::<Index<'_>>().unwrap();

        let actual: Vec<_> = index.iter().collect();
        assert_eq!(actual, &[&[5]]);

        // Write
        let mut ctxt = WriteBuffer::new();
        Index::write(&mut ctxt, &index).unwrap();

        assert_eq!(ctxt.bytes(), &[0, 1, 3, 0, 0, 1, 0, 0, 2, 5]);
    }

    #[test]
    fn test_write_int_operand() {
        assert_eq!(write_int_operand(0), &[0x8b]);
        assert_eq!(write_int_operand(100), &[0xef]);
        assert_eq!(write_int_operand(-100), &[0x27]);
        assert_eq!(write_int_operand(1000), &[0xfa, 0x7c]);
        assert_eq!(write_int_operand(-1000), &[0xfe, 0x7c]);
        assert_eq!(write_int_operand(10000), &[0x1c, 0x27, 0x10]);
        assert_eq!(write_int_operand(-10000), &[0x1c, 0xd8, 0xf0]);
        assert_eq!(write_int_operand(100000), &[0x1d, 0x00, 0x01, 0x86, 0xa0]);
        assert_eq!(write_int_operand(-100000), &[0x1d, 0xff, 0xfe, 0x79, 0x60]);
    }

    #[test]
    fn test_write_int_operand_round_trip() {
        int_operand_round_trip(0);
        int_operand_round_trip(100);
        int_operand_round_trip(540);
        int_operand_round_trip(-100);
        int_operand_round_trip(-267);
        int_operand_round_trip(1000);
        int_operand_round_trip(-1000);
        int_operand_round_trip(10000);
        int_operand_round_trip(-10000);
        int_operand_round_trip(100000);
        int_operand_round_trip(-100000);
    }

    fn int_operand_round_trip(val: i32) {
        let int = write_int_operand(val);
        match ReadScope::new(&int).read::<Op>().unwrap() {
            Op::Operand(Operand::Integer(actual)) => assert_eq!(actual, val),
            _ => unreachable!(),
        }
    }

    fn write_int_operand(val: i32) -> Vec<u8> {
        let mut ctxt = WriteBuffer::new();
        Operand::write(&mut ctxt, &Operand::Integer(val)).unwrap();
        ctxt.into_inner()
    }

    #[test]
    fn test_fd_select_font_dict_index_format0() {
        let glyph_font_dict_indices = ReadArrayCow::Owned(vec![1, 2, 3]);
        let fd_select = FDSelect::Format0 {
            glyph_font_dict_indices,
        };

        assert_eq!(fd_select.font_dict_index(2), Some(3));
        assert_eq!(fd_select.font_dict_index(3), None);
    }

    #[test]
    fn test_fd_select_font_dict_index_format3() {
        // Set up 3 ranges:
        //  0..10 -> Font DICT index 2
        // 10..17 -> Font DICT index 1
        // 17..33 -> Font DICT index 0
        let ranges: Vec<Range<u16, u8>> = vec![
            Range {
                first: 0,
                n_left: 2,
            },
            Range {
                first: 10,
                n_left: 1,
            },
            Range {
                first: 17,
                n_left: 0,
            },
        ];
        let fd_select = FDSelect::Format3 {
            ranges: ReadArrayCow::Owned(ranges),
            sentinel: 33,
        };

        assert_eq!(fd_select.font_dict_index(2), Some(2));
        assert_eq!(fd_select.font_dict_index(10), Some(1));
        assert_eq!(fd_select.font_dict_index(32), Some(0));
        assert_eq!(fd_select.font_dict_index(33), None);
    }

    #[test]
    fn test_charset_id_for_glyph_pre_defined_charsets() {
        assert_eq!(Charset::ISOAdobe.id_for_glyph(2), Some(2));
        assert_eq!(Charset::ISOAdobe.id_for_glyph(300), None);
        assert_eq!(Charset::Expert.id_for_glyph(2), Some(229));
        assert_eq!(Charset::Expert.id_for_glyph(300), None);
        assert_eq!(Charset::ExpertSubset.id_for_glyph(2), Some(231));
        assert_eq!(Charset::ExpertSubset.id_for_glyph(300), None);
    }

    #[test]
    fn test_custom_charset_id_for_glyph_format0() {
        let glyph_sids = ReadArrayCow::Owned(vec![1, 2, 3]);
        let charset = CustomCharset::Format0 { glyphs: glyph_sids };

        // glpyh id 0 is .notdef and is implicitly encoded
        assert_eq!(charset.id_for_glyph(0), Some(0));
        assert_eq!(charset.id_for_glyph(1), Some(1));
        assert_eq!(charset.id_for_glyph(4), None);
    }

    #[test]
    fn test_custom_charset_id_for_glyph_format1() {
        let ranges = ReadArrayCow::Owned(vec![Range {
            first: 34,
            n_left: 5,
        }]);
        let charset = CustomCharset::Format1 { ranges };

        // glpyh id 0 is .notdef and is implicitly encoded
        assert_eq!(charset.id_for_glyph(0), Some(0));
        assert_eq!(charset.id_for_glyph(1), Some(34));
        assert_eq!(charset.id_for_glyph(6), Some(39));
        assert_eq!(charset.id_for_glyph(7), None);
    }

    #[test]
    fn test_custom_charset_id_for_glyph_format2() {
        let ranges = ReadArrayCow::Owned(vec![Range {
            first: 34,
            n_left: 5,
        }]);
        let charset = CustomCharset::Format2 { ranges };

        // glpyh id 0 is .notdef and is implicitly encoded
        assert_eq!(charset.id_for_glyph(0), Some(0));
        assert_eq!(charset.id_for_glyph(1), Some(34));
        assert_eq!(charset.id_for_glyph(6), Some(39));
        assert_eq!(charset.id_for_glyph(7), None);
    }

    #[test]
    fn test_arno_custom_charset_ranges() {
        // These ranges are from the ArnoPro-Regular font and are in the same order they are in the
        // font.
        #[rustfmt::skip]
        let ranges = ReadArrayCow::Owned(vec![
            Range { first: 1, n_left: 107, },
            Range { first: 111, n_left: 38, },
            Range { first: 151, n_left: 12, },
            Range { first: 165, n_left: 3, },
            Range { first: 170, n_left: 58, },
            Range { first: 237, n_left: 1, },
            Range { first: 391, n_left: 0, },
            Range { first: 393, n_left: 0, },
            Range { first: 300, n_left: 0, },
            Range { first: 392, n_left: 0, },
            Range { first: 314, n_left: 0, },
            Range { first: 324, n_left: 1, },
            Range { first: 320, n_left: 3, },
            Range { first: 394, n_left: 2577, },
            Range { first: 109, n_left: 1, },
            Range { first: 2972, n_left: 28, },
            Range { first: 2846, n_left: 768, },
        ]);
        let charset = CustomCharset::Format2 { ranges };

        // glpyh id 0 is .notdef and is implicitly encoded
        assert_eq!(charset.id_for_glyph(134), Some(136));
        assert_eq!(charset.id_for_glyph(265), Some(422));
        assert_eq!(charset.id_for_glyph(279), Some(436));
    }

    #[test]
    fn test_custom_charset_iter() {
        // These ranges are from the ArnoPro-Regular font and are in the same order they are in the
        // font.
        #[rustfmt::skip]
            let ranges = ReadArrayCow::Owned(vec![
            Range { first: 111, n_left: 4, },
            Range { first: 1, n_left: 3, },
            Range { first: 2972, n_left: 2, },
        ]);
        let charset = CustomCharset::Format2 { ranges };
        let actual = charset.iter().collect_vec();
        let expected = vec![0, 111, 112, 113, 114, 115, 1, 2, 3, 4, 2972, 2973, 2974];

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_read_standard_string() {
        let data = b"Ferris";
        let offsets = [1, (data.len() + 1) as u8];
        let string_index = MaybeOwnedIndex::Borrowed(Index {
            count: 1,
            off_size: 1,
            offset_array: &offsets,
            data_array: data,
        });

        assert_eq!(
            read_string_index_string(&string_index, 7).unwrap(),
            "ampersand"
        );
        assert_eq!(
            read_string_index_string(&string_index, 390).unwrap(),
            "Semibold"
        );
    }

    #[test]
    fn test_read_custom_string() {
        let data = b"Ferris";
        let offsets = [1, (data.len() + 1) as u8];
        let string_index = MaybeOwnedIndex::Borrowed(Index {
            count: 1,
            off_size: 1,
            offset_array: &offsets,
            data_array: data,
        });

        assert_eq!(
            read_string_index_string(&string_index, 391).unwrap(),
            "Ferris"
        );
        assert!(read_string_index_string(&string_index, 392).is_err());
    }
}
