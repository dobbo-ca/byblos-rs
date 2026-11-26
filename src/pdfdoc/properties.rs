//! Port of `internal/pdfdoc/properties.go`: the Info-dictionary property seam.
//!
//! [`write_properties`] and [`read_properties`] are a thin wrapper over pdf-rs's own
//! `properties_add` and `properties`: the byblos package marshals a `Provenance` record to JSON
//! and hands it here as a single key/value pair, so this module never needs to know that
//! record's shape.
//!
//! Unlike [`super::open`], these validate. pdf-go's `api.AddProperties` and `api.Properties`
//! both call `ReadValidateAndOptimize` internally and neither offers a non-validating path.
//! Relaxed validation is the least strict mode pdf-go has, but a document `open` accepts is not
//! guaranteed to survive here.
//!
//! Both take the caps of `lim` for the parse (`Limits::pdf_config`), as Go's `defaultConfig`
//! takes the package defaults.

use super::deterministic::write_deterministic;
use super::doc::{conv, default_config};
use crate::error::Result;
use crate::limits::Limits;
use crate::optimize::write_env;
use pdf::model::XRefTable;
use pdf::object::text::{hex_literal_to_string, string_literal_to_string};
use pdf::object::{Dict, Object};
use pdf::{Cmd, Config, GoTime};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

/// `WriteProperties` adds `props` to the Info dictionary of the PDF `data` and returns the
/// result, replacing any existing entries with the same keys.
///
/// It makes the calls of `api.AddProperties` (`ReadValidateAndOptimize`, `PropertiesAdd`,
/// write) directly rather than through it, for the same reason as `optimize` (byb-c53): the
/// write must go through `write_deterministic`. `api.AddProperties`' remaining step, rejecting
/// blank property keys and values, is dropped: byblos' one caller passes a fixed key and
/// marshalled JSON, neither ever blank.
///
/// The clock that `properties_add` reads stamps nothing that survives: the dates and the file
/// identifier are put back below, so the output does not depend on it.
///
/// # Errors
/// The read, validate, optimize and property errors of pdf-rs ([`crate::Error::Pdf`]), a refused
/// allocation, [`crate::Error::Cancelled`], and the errors of the deterministic writer.
pub fn write_properties(
    data: &[u8],
    props: &BTreeMap<String, String>,
    lim: &Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    write_properties_at(data, props, None, lim, cancel)
}

/// [`write_properties`] with the fallback writer's clock pinned to `now` (parity random source);
/// `None` is the real clock and random source.
///
/// # Errors
/// As [`write_properties`].
pub fn write_properties_at(
    data: &[u8],
    props: &BTreeMap<String, String>,
    now: Option<GoTime>,
    lim: &Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    let cfg = Config {
        cmd: Cmd::AddProperties,
        ..default_config(lim)
    };
    let mut ctx = pdf::api::read_validate_and_optimize(data, &cfg, cancel)
        .map_err(|e| conv("write properties", e))?;
    let (creation, modified) = info_dates(&mut ctx.xref);
    let id = ctx.xref.id.clone();
    let env = write_env(now);
    pdf::api::properties_add(&mut ctx, props, &env, cancel)
        .map_err(|e| conv("write properties", e))?;
    // `properties_add` runs pdf-go's ensureInfoDictAndFileID, which stamps wall-clock
    // CreationDate/ModDate and a time-derived /ID onto the live context (measured: 6 runs, 6
    // different outputs without this). Restore the input's own state so the deterministic
    // writer pins from input data, not from this call's clock readings.
    ctx.xref.id = id;
    if let Some(info) = ctx.xref.info
        && let Ok(Some(d)) = ctx.xref.dereference_dict_mut(info)
    {
        restore_date(d, b"CreationDate", &creation);
        restore_date(d, b"ModDate", &modified);
    }
    write_deterministic(&mut ctx, &env, cancel)
}

/// `infoDates` returns the Info dictionary's CreationDate and ModDate as raw date strings, empty
/// for absent or unreadable. `deterministic.rs` has a private copy of this pair of functions
/// (`info_dates`, `raw_date`); they are Go's `infoDates` and `rawDate`, 25 lines.
fn info_dates(xt: &mut XRefTable) -> (String, String) {
    let Some(ir) = xt.info else {
        return (String::new(), String::new());
    };
    let (creation, modified) = match xt.dereference_dict_ref(ir) {
        Ok(Some(d)) => (
            d.0.get(&b"CreationDate"[..]).cloned(),
            d.0.get(&b"ModDate"[..]).cloned(),
        ),
        _ => return (String::new(), String::new()),
    };
    (raw_date(xt, creation), raw_date(xt, modified))
}

/// `rawDate`: the text of a string or hex string `o` (through a reference), empty otherwise.
fn raw_date(xt: &mut XRefTable, o: Option<Object>) -> String {
    let Some(o) = o else {
        return String::new();
    };
    match xt.dereference(&o) {
        Ok(Object::String(s)) => string_literal_to_string(s).unwrap_or_default(),
        Ok(Object::Hex(h)) => hex_literal_to_string(h).unwrap_or_default(),
        _ => String::new(),
    }
}

/// `restoreDate` puts the input's raw date back, or removes the key so the deterministic
/// writer's `pinInfo` falls back to its documented constant.
fn restore_date(d: &mut Dict, key: &[u8], raw: &str) {
    if raw.is_empty() {
        d.delete(key);
        return;
    }
    d.set(key, Object::String(raw.as_bytes().to_vec()));
}

/// `ReadProperties` returns the Info-dictionary properties of the PDF `data`. A key
/// [`write_properties`] never wrote is simply absent from the result, not an error.
///
/// # Errors
/// The read, validate and optimize errors of pdf-rs ([`crate::Error::Pdf`]), a refused
/// allocation, [`crate::Error::Cancelled`].
pub fn read_properties(
    data: &[u8],
    lim: &Limits,
    cancel: &AtomicBool,
) -> Result<BTreeMap<String, String>> {
    read_properties_with_mem(data, lim, &lim.mem(), cancel)
}

/// [`read_properties`] on the caller's ledger `mem`.
///
/// # Errors
/// As [`read_properties`].
pub(crate) fn read_properties_with_mem(
    data: &[u8],
    lim: &Limits,
    mem: &pdf::Mem,
    cancel: &AtomicBool,
) -> Result<BTreeMap<String, String>> {
    pdf::api::properties_with_mem(data, &default_config(lim), mem, cancel)
        .map_err(|e| conv("read properties", e))
}
