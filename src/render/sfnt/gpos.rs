//! Port of the parse-time half of golang.org/x/image/font/sfnt `gpos.go`: `parseGPOSKern`.
//!
//! `Font.Parse` walks the GPOS table for kerning and fails on a malformed one, so the walk and
//! every check it makes are kept. The kerning lookup closures Go builds from the walk (used only
//! by `Font.Kern`, which is not ported) are not.

use super::{
    Budget, INVALID_GPOS_TABLE, Result, Table, UNSUPPORTED_CLASS_DEF_FORMAT,
    UNSUPPORTED_COVERAGE_FORMAT, UNSUPPORTED_EXTENSION_POS_FORMAT, UNSUPPORTED_GPOS_TABLE, tail,
    try_u16, try_u32, u16, u32, var_len_view, view,
};

const HEX_SCRIPT_LATN: u32 = 0x6c61_746e; // latn
const HEX_SCRIPT_DFLT: u32 = 0x4446_4c54; // DFLT
const HEX_FEATURE_KERN: u32 = 0x6b65_726e; // kern

/// `parseGPOSKern`.
pub(crate) fn parse_gpos_kern(src: &[u8], gpos: &Table, budget: &mut Budget) -> Result<()> {
    if gpos.length == 0 {
        return Ok(());
    }
    // The GPOS header v1.1 is 14 bytes, but FeatureVariations are not supported.
    const HEADER_SIZE: u32 = 10;
    if gpos.length < HEADER_SIZE {
        return Err(INVALID_GPOS_TABLE);
    }

    let gpos_offset = i64::from(gpos.offset);
    let buf = view(src, gpos_offset, i64::from(HEADER_SIZE))?;

    // Check for version 1.0/1.1.
    if u16(buf) != 1 || u16(tail(buf, 2)?) > 1 {
        return Err(UNSUPPORTED_GPOS_TABLE);
    }
    let script_list_offset = i64::from(u16(tail(buf, 4)?));
    let feature_list_offset = i64::from(u16(tail(buf, 6)?));
    let lookup_list_offset = i64::from(u16(tail(buf, 8)?));

    // Get all feature indices for the latn script.
    let mut feature_idxs = parse_script_features(
        src,
        gpos_offset + script_list_offset,
        HEX_SCRIPT_LATN,
        budget,
    )?;
    if feature_idxs.is_empty() {
        // Get all feature indices for the DFLT script.
        feature_idxs = parse_script_features(
            src,
            gpos_offset + script_list_offset,
            HEX_SCRIPT_DFLT,
            budget,
        )?;
        if feature_idxs.is_empty() {
            return Ok(());
        }
    }

    // Get all lookup indices for the kern features.
    let lookup_idx = parse_features_lookup(
        src,
        gpos_offset + feature_list_offset,
        &feature_idxs,
        HEX_FEATURE_KERN,
        budget,
    )?;

    // LookupTableList: lookupCount, []lookups.
    let (lookup_list, num_lookup_tables) =
        var_len_view(src, gpos_offset + lookup_list_offset, 2, 0, 2)?;

    'lookup_tables: for &n in &lookup_idx {
        if n > num_lookup_tables {
            return Err(INVALID_GPOS_TABLE);
        }
        // Go indexes the lookup list at n == numLookupTables here and panics.
        let table_offset =
            gpos_offset + lookup_list_offset + i64::from(try_u16(lookup_list, 2 + n * 2)?);

        // LookupTable: lookupType, lookupFlag, subTableCount, []subtableOffsets,
        // markFilteringSet.
        let (buf, num_sub_tables) = var_len_view(src, table_offset, 8, 4, 2)?;

        let flags = u16(tail(buf, 2)?);

        let mut sub_table_offsets: Vec<i64> = Vec::new();
        sub_table_offsets.try_reserve_exact(num_sub_tables as usize)?;
        for i in 0..num_sub_tables {
            sub_table_offsets.push(table_offset + i64::from(u16(tail(buf, 6 + i * 2)?)));
        }

        match u16(buf) {
            // PairPos table.
            2 => {}
            9 => {
                // The Extension Positioning table defines an additional u32 offset to allow
                // subtables to exceed the 16-bit limit.
                for o in &mut sub_table_offsets {
                    let ext = view(src, *o, 8)?;
                    if u16(ext) != 1 {
                        return Err(UNSUPPORTED_EXTENSION_POS_FORMAT);
                    }
                    if u16(tail(ext, 2)?) != 2 {
                        continue 'lookup_tables;
                    }
                    *o += i64::from(u32(tail(ext, 4)?));
                }
            }
            // Other types are not supported.
            _ => continue,
        }

        if flags & 0x0010 > 0 {
            // useMarkFilteringSet is enabled: not supported.
            continue;
        }

        for &sub_table_offset in &sub_table_offsets {
            let hdr = view(src, sub_table_offset, 4)?;
            let format = u16(hdr);

            check_coverage_lookup(src, sub_table_offset + i64::from(u16(tail(hdr, 2)?)))?;

            match format {
                // Adjustments for glyph pairs.
                1 => parse_pair_pos_format1(src, sub_table_offset)?,
                // Class pair adjustment.
                2 => parse_pair_pos_format2(src, sub_table_offset)?,
                _ => {}
            }
        }
    }

    Ok(())
}

/// `parsePairPosFormat1`.
fn parse_pair_pos_format1(src: &[u8], offset: i64) -> Result<()> {
    // PairPos Format 1: posFormat, coverageOffset, valueFormat1, valueFormat2, pairSetCount,
    // []pairSetOffsets.
    let (buf, n_pairs) = var_len_view(src, offset, 10, 8, 2)?;
    // Only kerning with X_ADVANCE for the first glyph is supported.
    if u16(tail(buf, 4)?) != 0x04 || u16(tail(buf, 6)?) != 0x00 {
        return Ok(());
    }

    // A PairPos table holds offsets to PairSet tables, which hold PairValueRecords. The length
    // of the whole table is found by jumping to the last PairSet; the offsets are not sorted and
    // can repeat, so all of them are visited.
    let mut last_pair_set_offset = 0i64;
    for n in 0..n_pairs {
        let pair_offset = i64::from(try_u16(buf, 10 + n * 2)?);
        if pair_offset > last_pair_set_offset {
            last_pair_set_offset = pair_offset;
        }
    }
    let last = view(src, offset + last_pair_set_offset, 2)?;

    let pair_value_count = i64::from(u16(last));
    // Each PairSet holds the secondGlyph (u16) and one value record (X_ADVANCE).
    let last_pair_set_length = 2 + pair_value_count * 4;

    let length = last_pair_set_offset + last_pair_set_length;
    view(src, offset, length)?;
    Ok(())
}

/// `parsePairPosFormat2`.
fn parse_pair_pos_format2(src: &[u8], offset: i64) -> Result<()> {
    // PairPos Format 2: posFormat, coverageOffset, valueFormat1, valueFormat2, classDef1Offset,
    // classDef2Offset, class1Count, class2Count, []class1Records.
    let buf = view(src, offset, 16)?;
    // Only kerning with X_ADVANCE for the first glyph is supported.
    if u16(tail(buf, 4)?) != 0x04 || u16(tail(buf, 6)?) != 0x00 {
        return Ok(());
    }
    let num_class1 = i64::from(u16(tail(buf, 12)?));
    let num_class2 = i64::from(u16(tail(buf, 14)?));
    let cdef1_offset = offset + i64::from(u16(tail(buf, 8)?));
    let cdef2_offset = offset + i64::from(u16(tail(buf, 10)?));
    check_class_lookup(src, cdef1_offset)?;
    check_class_lookup(src, cdef2_offset)?;

    view(src, offset + 16, num_class1 * num_class2 * 2)?;
    Ok(())
}

/// `parseGPOSScriptFeatures`: all indices of features in the FeatureTable that are valid for
/// `script`, from its DefaultLangSys (other languages are not supported).
fn parse_script_features(
    src: &[u8],
    offset: i64,
    script: u32,
    budget: &mut Budget,
) -> Result<Vec<i64>> {
    // ScriptList table: scriptCount, []scriptRecords{scriptTag, scriptOffset}.
    let (buf, num_script_tables) = var_len_view(src, offset, 2, 0, 6)?;

    // Search the ScriptTables for the script.
    let mut script_table_offset = 0u16;
    for i in 0..num_script_tables {
        if try_u32(buf, 2 + i * 6)? == script {
            script_table_offset = try_u16(buf, 2 + i * 6 + 4)?;
            break;
        }
    }
    if script_table_offset == 0 {
        return Ok(Vec::new());
    }

    // Script table: defaultLangSys, langSysCount, []langSysRecords{langSysTag, langSysOffset}.
    let buf = view(src, offset + i64::from(script_table_offset), 2)?;
    let default_lang_sys_offset = u16(buf);
    if default_lang_sys_offset == 0 {
        return Ok(Vec::new());
    }

    // LangSys table: lookupOrder (reserved), requiredFeatureIndex, featureIndexCount,
    // []featureIndices.
    let (buf, num_features) = var_len_view(
        src,
        offset + i64::from(script_table_offset) + i64::from(default_lang_sys_offset),
        6,
        4,
        2,
    )?;

    let mut feature_idxs = budget.vec::<i64>(num_features as usize)?;
    for i in 0..num_features {
        feature_idxs.push(i64::from(try_u16(buf, 6 + i * 2)?));
    }
    Ok(feature_idxs)
}

/// `parseGPOSFeaturesLookup`: the lookup indices of the features in `feature_idxs` tagged
/// `feature`.
fn parse_features_lookup(
    src: &[u8],
    offset: i64,
    feature_idxs: &[i64],
    feature: u32,
    budget: &mut Budget,
) -> Result<Vec<i64>> {
    // FeatureList table: featureCount, []featureRecords{featureTag, featureOffset}.
    let (buf, num_feature_tables) = var_len_view(src, offset, 2, 0, 6)?;

    let mut lookup_idx: Vec<i64> = Vec::new();

    for &fidx in feature_idxs {
        if fidx > num_feature_tables {
            return Err(INVALID_GPOS_TABLE);
        }
        // Go indexes the feature list at fidx == featureCount here and panics.
        let feature_tag = try_u32(buf, 2 + fidx * 6)?;
        if feature_tag != feature {
            continue;
        }
        let feature_offset = i64::from(try_u16(buf, 2 + fidx * 6 + 4)?);

        let (lookups, num_lookups) = var_len_view(src, offset + feature_offset, 4, 2, 2)?;

        budget.charge(num_lookups as u64 * std::mem::size_of::<i64>() as u64)?;
        lookup_idx.try_reserve(num_lookups as usize)?;
        for i in 0..num_lookups {
            lookup_idx.push(i64::from(try_u16(lookups, 4 + i * 2)?));
        }
    }

    Ok(lookup_idx)
}

/// `makeCachedCoverageLookup`, validation only.
fn check_coverage_lookup(src: &[u8], offset: i64) -> Result<()> {
    let buf = view(src, offset, 2)?;
    match u16(buf) {
        // Coverage Format 1: coverageFormat, glyphCount, []glyphArray.
        1 => var_len_view(src, offset, 4, 2, 2).map(|_| ()),
        // Coverage Format 2: coverageFormat, rangeCount, []rangeRecords{startGlyphID,
        // endGlyphID, startCoverageIndex}.
        2 => var_len_view(src, offset, 4, 2, 6).map(|_| ()),
        _ => Err(UNSUPPORTED_COVERAGE_FORMAT),
    }
}

/// `makeCachedClassLookup`, validation only.
fn check_class_lookup(src: &[u8], offset: i64) -> Result<()> {
    let buf = view(src, offset, 2)?;
    match u16(buf) {
        // ClassDefFormat 1: classFormat, startGlyphID, glyphCount, []classValueArray.
        1 => var_len_view(src, offset, 6, 4, 2).map(|_| ()),
        // ClassDefFormat 2: classFormat, classRangeCount, []classRangeRecords.
        2 => var_len_view(src, offset, 4, 2, 6).map(|_| ()),
        _ => Err(UNSUPPORTED_CLASS_DEF_FORMAT),
    }
}
