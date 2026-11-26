//! The `build` operations of the parity oracle (docs/parity.md, "Operations"): `embed` the
//! image, then `BuildPDFLimits` of one page, for `.png`, `.jpg` and `.jp2` inputs. Mirrors
//! `opBuild` and `buildFromImage` of the Go oracle's `ops.go`.

use crate::common::{base_lim, embed_any, emit, ext_of, raw, tight_lim};
use byblos::build::{BuildPage, build_pdf};
use byblos::embed::exif_rotation;
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// Runs this area's operations on one input and writes `<name>.<op>.out` or `.err` files to `out`.
pub fn run(name: &str, data: &[u8], threads: usize, out: &Path) -> std::io::Result<()> {
    let ext = ext_of(name);
    if !matches!(ext.as_str(), "png" | "jpg" | "jp2") {
        return Ok(());
    }
    let cancel = AtomicBool::new(false);
    for (sfx, lim) in [("", base_lim(threads)), ("-tight", tight_lim(threads))] {
        emit(out, name, &format!("build{sfx}"), || {
            let (image, res) = embed_any(&ext, data, &lim)?;
            // Rotate is EXIFRotationLimits, 0 when that fails.
            let rotate = exif_rotation(data, &lim).map_or(0, i64::from);
            let page = BuildPage {
                image,
                dpi: res.x,
                dpi_y: res.y,
                rotate,
                ..BuildPage::default()
            };
            Ok(Some(raw(&build_pdf(&[page], &lim, &cancel)?)))
        })?;
    }
    Ok(())
}
