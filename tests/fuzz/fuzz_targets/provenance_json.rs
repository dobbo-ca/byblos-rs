#![no_main]
#[path = "../common.rs"]
mod common;
use byblos::provenance::Provenance;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let lim = common::limits();
    if let Ok(p) = Provenance::from_json(data, &lim) {
        let _ = p.to_json(&lim);
    }
});
