#![no_main]
#[path = "../common.rs"]
mod common;
use byblos::provenance::{read_provenance, record_extraction};
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let lim = common::limits();
    if let Err(e) = read_provenance(data, &lim, &common::NEVER) {
        common::report(&e);
    }
    if let Err(e) = record_extraction(data, None, &lim, &common::NEVER) {
        common::report(&e);
    }
});
