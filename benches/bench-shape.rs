use allsorts::binary::read::ReadScope;
use allsorts::error::{ParseError, ShapingError};
use allsorts::font_data_impl::read_cmap_subtable;
use allsorts::gpos::{gpos_apply, Info};
use allsorts::gsub::{gsub_apply_default, GlyphOrigin, RawGlyph};
use allsorts::layout::{new_layout_cache, GDEFTable, LayoutTable, GPOS, GSUB};
use allsorts::tables::cmap::{Cmap, CmapSubtable};
use allsorts::tables::{MaxpTable, OffsetTable, OpenTypeFile, OpenTypeFont, TTCHeader};
use allsorts::tag;

use std::convert::TryFrom;

use std::path::Path;

use criterion::{criterion_group, criterion_main, Criterion};

fn shape<P: AsRef<Path>>(filename: P, script: u32, lang: u32, text: &str) {
    let buffer = std::fs::read(filename).unwrap();
    let fontfile = ReadScope::new(&buffer).read::<OpenTypeFile>().unwrap();

    match fontfile.font {
        OpenTypeFont::Single(ttf) => shape_ttf(&fontfile.scope, ttf, script, lang, text).unwrap(),
        OpenTypeFont::Collection(ttc) => {
            shape_ttc(fontfile.scope, ttc, script, lang, text).unwrap()
        }
    }
}

fn shape_ttc<'a>(
    scope: ReadScope<'a>,
    ttc: TTCHeader<'a>,
    script: u32,
    lang: u32,
    text: &str,
) -> Result<(), ShapingError> {
    for offset_table_offset in &ttc.offset_tables {
        let offset_table_offset = usize::try_from(offset_table_offset)?;
        let offset_table = scope.offset(offset_table_offset).read::<OffsetTable>()?;
        shape_ttf(&scope, offset_table, script, lang, text)?;
    }
    Ok(())
}

fn shape_ttf<'a>(
    scope: &ReadScope<'a>,
    ttf: OffsetTable<'a>,
    script: u32,
    lang: u32,
    text: &str,
) -> Result<(), ShapingError> {
    let cmap = if let Some(cmap_scope) = ttf.read_table(&scope, tag::CMAP)? {
        cmap_scope.read::<Cmap>()?
    } else {
        println!("no cmap table");
        return Ok(());
    };
    let (_, cmap_subtable) = if let Some(cmap_subtable) = read_cmap_subtable(&cmap)? {
        cmap_subtable
    } else {
        println!("no suitable cmap subtable");
        return Ok(());
    };
    let num_glyphs = match ttf.read_table(&scope, tag::MAXP)? {
        Some(maxp_scope) => {
            let maxp = maxp_scope.read::<MaxpTable>()?;
            maxp.num_glyphs
        }
        None => {
            println!("no maxp table");
            return Ok(());
        }
    };
    let opt_glyphs_res: Result<Vec<_>, _> = text
        .chars()
        .map(|ch| map_glyph(&cmap_subtable, ch))
        .collect();
    let opt_glyphs = opt_glyphs_res?;
    let mut glyphs = opt_glyphs.into_iter().flatten().collect();
    if let Some(gsub_record) = ttf.find_table_record(tag::GSUB) {
        let gsub_table = gsub_record
            .read_table(&scope)?
            .read::<LayoutTable<GSUB>>()?;
        let opt_gdef_table = match ttf.find_table_record(tag::GDEF) {
            Some(gdef_record) => Some(gdef_record.read_table(&scope)?.read::<GDEFTable>()?),
            None => None,
        };
        let opt_gpos_table = match ttf.find_table_record(tag::GPOS) {
            Some(gpos_record) => Some(
                gpos_record
                    .read_table(&scope)?
                    .read::<LayoutTable<GPOS>>()?,
            ),
            None => None,
        };
        let common_ligatures = true;
        let discretionary_ligatures = false;
        let historical_ligatures = false;
        let contextual_ligatures = true;
        let vertical = false;
        let gsub_cache = new_layout_cache(gsub_table);
        let _res = gsub_apply_default(
            &|| make_dotted_circle(&cmap_subtable),
            &gsub_cache,
            opt_gdef_table.as_ref(),
            script,
            lang,
            common_ligatures,
            discretionary_ligatures,
            historical_ligatures,
            contextual_ligatures,
            vertical,
            num_glyphs,
            &mut glyphs,
        )?;

        match opt_gpos_table {
            Some(gpos_table) => {
                let kerning = true;
                let mut infos = Info::init_from_glyphs(opt_gdef_table.as_ref(), glyphs)?;
                let gpos_cache = new_layout_cache(gpos_table);
                gpos_apply(
                    &gpos_cache,
                    opt_gdef_table.as_ref(),
                    kerning,
                    script,
                    lang,
                    &mut infos,
                )?;
            }
            None => {}
        }
    } else {
        println!("no GSUB table");
    }
    Ok(())
}

fn make_dotted_circle(cmap_subtable: &CmapSubtable) -> Vec<RawGlyph<()>> {
    match map_glyph(cmap_subtable, '\u{25cc}') {
        Ok(Some(raw_glyph)) => vec![raw_glyph],
        _ => Vec::new(),
    }
}

fn map_glyph(cmap_subtable: &CmapSubtable, ch: char) -> Result<Option<RawGlyph<()>>, ParseError> {
    if let Some(glyph_index) = cmap_subtable.map_glyph(ch as u32)? {
        let glyph = make_glyph(ch, glyph_index);
        Ok(Some(glyph))
    } else {
        Ok(None)
    }
}

fn make_glyph(ch: char, glyph_index: u16) -> RawGlyph<()> {
    RawGlyph {
        unicodes: vec![ch],
        glyph_index: Some(glyph_index),
        liga_component_pos: 0,
        glyph_origin: GlyphOrigin::Char(ch),
        small_caps: false,
        multi_subst_dup: false,
        is_vert_alt: false,
        fake_bold: false,
        fake_italic: false,
        extra_data: (),
    }
}

fn criterion_benchmark(c: &mut Criterion) {
    c.bench_function("shape Hello World Noto Serif Regular", |b| {
        b.iter(|| {
            shape(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../data/fonts/noto/NotoSerif-Regular.ttf"),
                tag::DFLT,
                tag::LATN,
                "Hello World",
            )
        })
    });

    c.bench_function("shape FTL.txt Noto Serif Regular", |b| {
        b.iter(|| {
            shape(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../data/fonts/noto/NotoSerif-Regular.ttf"),
                tag::DFLT,
                tag::LATN,
                include_str!("../../../../data/doc/contrib/freetype/FTL.TXT"),
            )
        })
    });
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
