#![no_main]
#[path = "../common.rs"]
mod common;
use byblos::render::type1::{T1Error, T1Sink, parse_type1};
use libfuzzer_sys::fuzz_target;

struct Sink;

impl T1Sink for Sink {
    fn move_to(&mut self, _x: f64, _y: f64) -> Result<(), T1Error> {
        Ok(())
    }
    fn line_to(&mut self, _x: f64, _y: f64) -> Result<(), T1Error> {
        Ok(())
    }
    fn curve_to(&mut self, _: f64, _: f64, _: f64, _: f64, _: f64, _: f64) -> Result<(), T1Error> {
        Ok(())
    }
}

fuzz_target!(|data: &[u8]| {
    let Some(f) = parse_type1(data) else { return };
    for code in 0..=255u8 {
        if let Some(cs) = f.glyph(code) {
            let mut work = 0;
            let _ = f.interpret(cs, &mut Sink, &mut work);
            let _ = f.advance(code);
        }
    }
});
