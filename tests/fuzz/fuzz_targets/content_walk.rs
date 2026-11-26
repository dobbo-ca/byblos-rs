#![no_main]
#[path = "../common.rs"]
mod common;
use byblos::Limits;
use byblos::content::{BBox, Env, IDENTITY, XObject, walk};
use libfuzzer_sys::fuzz_target;
use std::sync::Arc;

// A mock resource tree: /Im* are images, /F* are forms whose content is the input itself (so the
// walk recurses to the depth and invocation caps), /F* are also the fonts.
struct Mock(Arc<Vec<u8>>);

impl Env for Mock {
    fn xobject(&self, _scope: i64, name: &[u8]) -> Option<XObject> {
        if name.starts_with(b"Im") {
            Some(XObject {
                image: true,
                id: 1,
                ..XObject::default()
            })
        } else if name.starts_with(b"F") {
            Some(XObject {
                image: false,
                id: 2,
                content: self.0.clone(),
                matrix: IDENTITY,
                scope: 1,
                bbox: Some(BBox {
                    llx: 0.0,
                    lly: 0.0,
                    urx: 612.0,
                    ury: 792.0,
                }),
            })
        } else {
            None
        }
    }
    fn ext_gstate_opaque(&self, _scope: i64, name: &[u8]) -> bool {
        name.len() % 2 == 0
    }
    fn font(&self, _scope: i64, name: &[u8]) -> Option<i64> {
        name.starts_with(b"F").then_some(7)
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 1 << 16 {
        return;
    }
    let lim = Limits {
        max_form_invocations: 16,
        ..common::limits()
    };
    let src = Arc::new(data.to_vec());
    let env = Mock(src.clone());
    let _ = walk(&common::NEVER, &src, 0, &env, &lim);
});
