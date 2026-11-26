//! Go 1.27's `image/png` reader tests (`reader_test.go`), ported. The fixtures are Go's
//! `testdata` (tests/data/png). `TestReader` compares each decoded pngsuite image with its `.sng`
//! golden through the same approximation of the `sng` tool that Go's test uses.

use byblos::image::{Color, ColorModel, Image, decode_config, png_read};
use byblos::{Error, Limits};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

const PNG_HEADER: &[u8] = b"\x89PNG\r\n\x1a\n";

const UNKNOWN_CHUNK: [u8; 47] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0x06, 0xf4, 0x7c, 0x55, 0x04, 0x1a, 0xd3, 0x11, 0x9a, 0x73, 0x00, 0x00, 0xf8, 0x1e, 0xf3, 0x2e,
    0x00, 0x00, 0x01, 0x00, 0xff, 0xff, 0xff, 0xff, 0x07, 0xf4, 0x7c, 0x55, 0x04, 0x1a, 0xd3,
];

const GRAY8_TRANSPARENT: [u8; 204] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x0f, 0x00, 0x00, 0x00, 0x0b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x85, 0x2c, 0x88,
    0x80, 0x00, 0x00, 0x00, 0x02, 0x74, 0x52, 0x4e, 0x53, 0x00, 0xff, 0x5b, 0x91, 0x22, 0xb5, 0x00,
    0x00, 0x00, 0x02, 0x62, 0x4b, 0x47, 0x44, 0x00, 0xff, 0x87, 0x8f, 0xcc, 0xbf, 0x00, 0x00, 0x00,
    0x09, 0x70, 0x48, 0x59, 0x73, 0x00, 0x00, 0x0a, 0xf0, 0x00, 0x00, 0x0a, 0xf0, 0x01, 0x42, 0xac,
    0x34, 0x98, 0x00, 0x00, 0x00, 0x07, 0x74, 0x49, 0x4d, 0x45, 0x07, 0xd5, 0x04, 0x02, 0x12, 0x11,
    0x11, 0xf7, 0x65, 0x3d, 0x8b, 0x00, 0x00, 0x00, 0x4f, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63,
    0xf8, 0xff, 0xff, 0xff, 0xb9, 0xbd, 0x70, 0xf0, 0x8c, 0x01, 0xc8, 0xaf, 0x6e, 0x99, 0x02, 0x05,
    0xd9, 0x7b, 0xc1, 0xfc, 0x6b, 0xff, 0xa1, 0xa0, 0x87, 0x30, 0xff, 0xd9, 0xde, 0xbd, 0xd5, 0x4b,
    0xf7, 0xee, 0xfd, 0x0e, 0xe3, 0xef, 0xcd, 0x06, 0x19, 0x14, 0xf5, 0x1e, 0xce, 0xef, 0x01, 0x31,
    0x92, 0xd7, 0x82, 0x41, 0x31, 0x9c, 0x3f, 0x07, 0x02, 0xee, 0xa1, 0xaa, 0xff, 0xff, 0x9f, 0xe1,
    0xd9, 0x56, 0x30, 0xf8, 0x0e, 0xe5, 0x03, 0x00, 0xa9, 0x42, 0x84, 0x3d, 0xdf, 0x8f, 0xa6, 0x8f,
    0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

const OVERFLOW_0: [u8; 78] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x7f, 0xff, 0xff, 0xfe, 0x7f, 0xff, 0xff, 0xfe, 0x08, 0x06, 0x00, 0x00, 0x00, 0x30, 0x57, 0xb3,
    0xfd, 0x00, 0x00, 0x00, 0x15, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x62, 0x62, 0x20, 0x12, 0x8c,
    0x2a, 0xa4, 0xb3, 0x42, 0x40, 0x00, 0x00, 0x00, 0xff, 0xff, 0x13, 0x38, 0x00, 0x15, 0x2d, 0xef,
    0x5f, 0x0f, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

const OVERFLOW_1: [u8; 78] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0xb5, 0x04, 0x00, 0x00, 0xb5, 0x04, 0x08, 0x06, 0x00, 0x00, 0x00, 0xf5, 0x60, 0x2c,
    0xb8, 0x00, 0x00, 0x00, 0x15, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x62, 0x62, 0x20, 0x12, 0x8c,
    0x2a, 0xa4, 0xb3, 0x42, 0x40, 0x00, 0x00, 0x00, 0xff, 0xff, 0x13, 0x38, 0x00, 0x15, 0x2d, 0xef,
    0x5f, 0x0f, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

const OVERFLOW_2: [u8; 78] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x30, 0x6e, 0xc5,
    0x21, 0x00, 0x00, 0x00, 0x15, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x62, 0x62, 0x20, 0x12, 0x8c,
    0x2a, 0xa4, 0xb3, 0x42, 0x40, 0x00, 0x00, 0x00, 0xff, 0xff, 0x13, 0x38, 0x00, 0x15, 0x2d, 0xef,
    0x5f, 0x0f, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

const OVERFLOW_3: [u8; 66] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0xaa, 0xd4, 0x7c,
    0xda, 0x00, 0x00, 0x00, 0x15, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x62, 0x66, 0x20, 0x12, 0x30,
    0x8d, 0x2a, 0xa4, 0xaf, 0x42, 0x40, 0x00, 0x00, 0x00, 0xff, 0xff, 0x14, 0xd2, 0x00, 0x16, 0x00,
    0x00, 0x00,
];

const PALETTED_TRNS: [u8; 383] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x20, 0x04, 0x03, 0x00, 0x00, 0x00, 0x81, 0x54, 0x67,
    0xc7, 0x00, 0x00, 0x00, 0x30, 0x50, 0x4c, 0x54, 0x45, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0x0e,
    0x00, 0x23, 0x27, 0x7b, 0xb1, 0x2d, 0x0a, 0x49, 0x3f, 0x19, 0x78, 0x5f, 0xcd, 0xe4, 0x69, 0x69,
    0xe4, 0x71, 0x59, 0x53, 0x80, 0x11, 0x14, 0x8b, 0x00, 0xa9, 0x8d, 0x95, 0xcb, 0x99, 0x2f, 0x6b,
    0xd7, 0x29, 0x91, 0xd7, 0x7b, 0xba, 0xff, 0xe3, 0xd7, 0x13, 0xc6, 0xd3, 0x58, 0x00, 0x00, 0x00,
    0x01, 0x74, 0x52, 0x4e, 0x53, 0x00, 0x40, 0xe6, 0xd8, 0x66, 0x00, 0x00, 0x00, 0xfd, 0x49, 0x44,
    0x41, 0x54, 0x28, 0xcf, 0x63, 0x60, 0x00, 0x83, 0x55, 0x0c, 0x68, 0x60, 0x9d, 0x02, 0x9a, 0x80,
    0xde, 0x23, 0x74, 0x15, 0xef, 0x50, 0x94, 0x70, 0x2d, 0xd2, 0x7b, 0x87, 0xa2, 0x84, 0xeb, 0xee,
    0xbb, 0x77, 0x6f, 0x51, 0x94, 0xe8, 0xbd, 0x7d, 0xf7, 0xee, 0x12, 0xb2, 0x80, 0xd2, 0x3d, 0x54,
    0x01, 0x26, 0x10, 0x1f, 0x59, 0x40, 0x0f, 0xc8, 0xd7, 0x7e, 0x84, 0x70, 0x1c, 0xd7, 0xba, 0xb7,
    0x4a, 0xda, 0xda, 0x77, 0x11, 0xf6, 0xac, 0x5a, 0xa5, 0xf4, 0xf9, 0xbf, 0xfd, 0x3d, 0x24, 0x6b,
    0x98, 0x94, 0xf4, 0xff, 0x7f, 0x52, 0x42, 0x16, 0x30, 0x0e, 0xd9, 0xed, 0x6a, 0x8c, 0xec, 0x10,
    0x65, 0x53, 0x97, 0x60, 0x23, 0x64, 0x1d, 0x8a, 0x2e, 0xc6, 0x2e, 0x42, 0x08, 0x3d, 0x4c, 0xca,
    0x81, 0xc1, 0x82, 0xa6, 0xa2, 0x46, 0x08, 0x3d, 0x4a, 0xa1, 0x82, 0xc6, 0x82, 0xa1, 0x4a, 0x08,
    0x3d, 0xfa, 0xa6, 0x81, 0xa1, 0xa2, 0xc1, 0x9f, 0x10, 0x66, 0xd4, 0x2b, 0x87, 0x0a, 0x86, 0x1a,
    0x7d, 0x57, 0x80, 0x9b, 0x99, 0xaf, 0x62, 0x1a, 0x1a, 0xec, 0xf0, 0x0d, 0x66, 0x2a, 0x7b, 0x5a,
    0xba, 0xd2, 0x64, 0x63, 0x4b, 0xa6, 0xb2, 0xb4, 0x02, 0xa8, 0x12, 0xb5, 0x24, 0xa5, 0x99, 0x2e,
    0x33, 0x95, 0xd4, 0x92, 0x10, 0xee, 0xd0, 0x59, 0xb9, 0x6a, 0xd6, 0x21, 0x24, 0xb7, 0x33, 0x9d,
    0x01, 0x01, 0x64, 0xbf, 0xac, 0x59, 0xb2, 0xca, 0xeb, 0x14, 0x92, 0x80, 0xd6, 0x9a, 0x53, 0x4a,
    0x6b, 0x4e, 0x2d, 0x42, 0x52, 0xa1, 0x73, 0x28, 0x54, 0xe7, 0x90, 0x6a, 0x00, 0x92, 0x92, 0x45,
    0xa1, 0x40, 0x84, 0x2c, 0xe0, 0xc4, 0xa0, 0xb2, 0x28, 0x14, 0xc1, 0x67, 0xe9, 0x50, 0x60, 0x60,
    0xea, 0x70, 0x40, 0x12, 0x00, 0x79, 0x54, 0x09, 0x22, 0x00, 0x00, 0x30, 0xf3, 0x52, 0x87, 0xc6,
    0xe4, 0xbd, 0x70, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/png")
        .join(rel)
}

fn decode(data: &[u8]) -> byblos::Result<Image> {
    png_read::decode(data, &Limits::default(), &AtomicBool::new(false))
}

fn read_png(rel: &str) -> byblos::Result<Image> {
    decode(&fs::read(path(rel)).unwrap())
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

const FILENAMES: [&str; 35] = [
    "basn0g01",
    "basn0g01-30",
    "basn0g02",
    "basn0g02-29",
    "basn0g04",
    "basn0g04-31",
    "basn0g08",
    "basn0g16",
    "basn2c08",
    "basn2c16",
    "basn3p01",
    "basn3p02",
    "basn3p04",
    "basn3p04-31i",
    "basn3p08",
    "basn3p08-trns",
    "basn4a08",
    "basn4a16",
    "basn6a08",
    "basn6a16",
    "ftbbn0g01",
    "ftbbn0g02",
    "ftbbn0g04",
    "ftbbn2c16",
    "ftbbn3p08",
    "ftbgn2c16",
    "ftbgn3p08",
    "ftbrn2c08",
    "ftbwn0g16",
    "ftbwn3p08",
    "ftbyn3p08",
    "ftp0n0g08",
    "ftp0n2c08",
    "ftp0n3p08",
    "ftp1n3p08",
];

const FILENAMES_PALETTED: [&str; 5] = [
    "basn3p01",
    "basn3p02",
    "basn3p04",
    "basn3p08",
    "basn3p08-trns",
];

/// `fakebKGDs`.
fn fake_bkgd(name: &str) -> Option<&'static str> {
    Some(match name {
        "ftbbn0g01" | "ftbbn0g02" | "ftbbn0g04" => "bKGD {gray: 0;}\n",
        "ftbbn2c16" => "bKGD {red: 0;  green: 0;  blue: 65535;}\n",
        "ftbbn3p08" | "ftbgn3p08" | "ftbyn3p08" => "bKGD {index: 245}\n",
        "ftbgn2c16" => "bKGD {red: 0;  green: 65535;  blue: 0;}\n",
        "ftbrn2c08" => "bKGD {red: 255;  green: 0;  blue: 0;}\n",
        "ftbwn0g16" => "bKGD {gray: 65535;}\n",
        "ftbwn3p08" => "bKGD {index: 0}\n",
        _ => return None,
    })
}

/// `fakegAMAs`, with the default `gAMA {1.0000}`.
fn fake_gama(name: &str) -> &'static str {
    match name {
        "ftbbn0g01" => "",
        "ftbbn0g02" => "gAMA {0.45455}\n",
        _ => "gAMA {1.0000}\n",
    }
}

/// `fakeIHDRUsings`.
fn fake_ihdr_using(name: &str) -> Option<&'static str> {
    Some(match name {
        "ftbbn0g01" | "ftbbn0g02" | "ftbbn0g04" | "ftbwn0g16" => "    using grayscale;\n",
        "ftbbn2c16" | "ftbgn2c16" | "ftbrn2c08" => "    using color;\n",
        _ => return None,
    })
}

fn model_of(img: &Image) -> ColorModel {
    match img {
        Image::Gray(_) => ColorModel::Gray,
        Image::Gray16(_) => ColorModel::Gray16,
        Image::Rgba(_) => ColorModel::Rgba,
        Image::Rgba64(_) => ColorModel::Rgba64,
        Image::Nrgba(_) => ColorModel::Nrgba,
        Image::Nrgba64(_) => ColorModel::Nrgba64,
        Image::Paletted(p) => ColorModel::Paletted(p.palette.clone()),
        _ => panic!("unexpected image type"),
    }
}

/// An approximation of the sng command-line tool (`sng` in `reader_test.go`).
fn sng(filename: &str, img: &Image) -> String {
    use std::fmt::Write;
    let mut w = String::new();
    let bounds = img.bounds();
    let cm = model_of(img);
    let mut bitdepth = match cm {
        ColorModel::Rgba | ColorModel::Nrgba | ColorModel::Gray => 8,
        _ => 16,
    };
    let cpm = match &cm {
        ColorModel::Paletted(p) => Some(p),
        _ => None,
    };
    if let Some(p) = cpm {
        bitdepth = match p.len() {
            0..=2 => 1,
            3..=4 => 2,
            5..=16 => 4,
            _ => 8,
        };
    }

    // Write the filename and IHDR.
    writeln!(w, "#SNG: from {filename}.png\nIHDR {{").unwrap();
    writeln!(
        w,
        "    width: {}; height: {}; bitdepth: {};",
        bounds.dx(),
        bounds.dy(),
        bitdepth
    )
    .unwrap();
    if let Some(s) = fake_ihdr_using(filename) {
        w.push_str(s);
    } else {
        w.push_str(match &cm {
            ColorModel::Rgba | ColorModel::Rgba64 => "    using color;\n",
            ColorModel::Nrgba | ColorModel::Nrgba64 => "    using color alpha;\n",
            ColorModel::Gray | ColorModel::Gray16 => "    using grayscale;\n",
            ColorModel::Paletted(_) => "    using color palette;\n",
            _ => "unknown PNG decoder color model\n",
        });
    }
    w.push_str("}\n");

    // We fake a gAMA chunk. The test files have a gAMA chunk but the PNG reader ignores it.
    w.push_str(fake_gama(filename));

    // Write the PLTE and tRNS (if applicable).
    let mut use_transparent = false;
    if let Some(p) = cpm {
        let mut last_alpha: Option<usize> = None;
        w.push_str("PLTE {\n");
        for (i, c) in p.iter().enumerate() {
            let (r, g, b, a) = match *c {
                Color::Rgba { r, g, b, .. } => (r, g, b, 0xff),
                Color::Nrgba { r, g, b, a } => (r, g, b, a),
                _ => panic!("unknown palette color type"),
            };
            if a != 0xff {
                last_alpha = Some(i);
            }
            writeln!(
                w,
                "    ({r:3},{g:3},{b:3})     # rgb = (0x{r:02x},0x{g:02x},0x{b:02x})"
            )
            .unwrap();
        }
        w.push_str("}\n");
        if let Some(s) = fake_bkgd(filename) {
            w.push_str(s);
        }
        if let Some(last) = last_alpha {
            w.push_str("tRNS {\n");
            for c in &p[..=last] {
                write!(w, " {}", c.rgba().3 >> 8).unwrap();
            }
            w.push_str("}\n");
        }
    } else if filename.starts_with("ft") {
        if let Some(s) = fake_bkgd(filename) {
            w.push_str(s);
        }
        // We fake a tRNS chunk. The test files' grayscale and truecolor transparent images all
        // have their top left corner transparent.
        match img.at(0, 0) {
            Color::Nrgba { r, g, b, a: 0 } => {
                use_transparent = true;
                w.push_str("tRNS {\n");
                match filename {
                    "ftbbn0g01" | "ftbbn0g02" | "ftbbn0g04" => {
                        writeln!(w, "    gray: {r};").unwrap();
                    }
                    _ => writeln!(w, "    red: {r}; green: {g}; blue: {b};").unwrap(),
                }
                w.push_str("}\n");
            }
            Color::Nrgba64 { r, g, b, a: 0 } => {
                use_transparent = true;
                w.push_str("tRNS {\n");
                match filename {
                    "ftbwn0g16" => writeln!(w, "    gray: {r};").unwrap(),
                    _ => writeln!(w, "    red: {r}; green: {g}; blue: {b};").unwrap(),
                }
                w.push_str("}\n");
            }
            _ => {}
        }
    }

    // Write the IMAGE.
    w.push_str("IMAGE {\n    pixels hex\n");
    for y in bounds.y0..bounds.y1 {
        match &cm {
            ColorModel::Gray => {
                for x in bounds.x0..bounds.x1 {
                    let Color::Gray(v) = img.at(x, y) else {
                        panic!("not gray")
                    };
                    write!(w, "{v:02x}").unwrap();
                }
            }
            ColorModel::Gray16 => {
                for x in bounds.x0..bounds.x1 {
                    let Color::Gray16(v) = img.at(x, y) else {
                        panic!("not gray16")
                    };
                    write!(w, "{v:04x} ").unwrap();
                }
            }
            ColorModel::Rgba => {
                for x in bounds.x0..bounds.x1 {
                    let Color::Rgba { r, g, b, .. } = img.at(x, y) else {
                        panic!("not rgba")
                    };
                    write!(w, "{r:02x}{g:02x}{b:02x} ").unwrap();
                }
            }
            ColorModel::Rgba64 => {
                for x in bounds.x0..bounds.x1 {
                    let Color::Rgba64 { r, g, b, .. } = img.at(x, y) else {
                        panic!("not rgba64")
                    };
                    write!(w, "{r:04x}{g:04x}{b:04x} ").unwrap();
                }
            }
            ColorModel::Nrgba => {
                for x in bounds.x0..bounds.x1 {
                    let Color::Nrgba { r, g, b, a } = img.at(x, y) else {
                        panic!("not nrgba")
                    };
                    let res = match filename {
                        "ftbbn0g01" | "ftbbn0g02" | "ftbbn0g04" => write!(w, "{r:02x}"),
                        _ if use_transparent => write!(w, "{r:02x}{g:02x}{b:02x} "),
                        _ => write!(w, "{r:02x}{g:02x}{b:02x}{a:02x} "),
                    };
                    res.unwrap();
                }
            }
            ColorModel::Nrgba64 => {
                for x in bounds.x0..bounds.x1 {
                    let Color::Nrgba64 { r, g, b, a } = img.at(x, y) else {
                        panic!("not nrgba64")
                    };
                    let res = match filename {
                        "ftbwn0g16" => write!(w, "{r:04x} "),
                        _ if use_transparent => write!(w, "{r:04x}{g:04x}{b:04x} "),
                        _ => write!(w, "{r:04x}{g:04x}{b:04x}{a:04x} "),
                    };
                    res.unwrap();
                }
            }
            ColorModel::Paletted(_) => {
                let Image::Paletted(p) = img else {
                    panic!("not paletted")
                };
                let (mut b, mut c) = (0usize, 0usize);
                for x in bounds.x0..bounds.x1 {
                    let idx = p.pix[(y as usize) * p.stride + x as usize];
                    b = (b << bitdepth) | usize::from(idx);
                    c += 1;
                    if c == 8 / bitdepth {
                        write!(w, "{b:02x}").unwrap();
                        b = 0;
                        c = 0;
                    }
                }
                if c != 0 {
                    while c != 8 / bitdepth {
                        b <<= bitdepth;
                        c += 1;
                    }
                    write!(w, "{b:02x}").unwrap();
                }
            }
            _ => {}
        }
        w.push('\n');
    }
    w.push_str("}\n");
    w
}

#[test]
fn reader() {
    for fn_ in FILENAMES {
        // Read the .png file.
        let img = read_png(&format!("pngsuite/{fn_}.png")).unwrap_or_else(|e| panic!("{fn_}: {e}"));

        if fn_ == "basn4a16" {
            // basn4a16.sng is gray + alpha but sng() will produce true color + alpha, so we just
            // check a single random pixel.
            assert_eq!(
                img.at(2, 1),
                Color::Nrgba64 {
                    r: 0x11a7,
                    g: 0x11a7,
                    b: 0x11a7,
                    a: 0x1085
                },
                "{fn_}: wrong pixel value at (2, 1)"
            );
            continue;
        }

        // Compare the two, in SNG format, line by line.
        let got = sng(fn_, &img);
        let want = fs::read_to_string(path(&format!("pngsuite/{fn_}.sng"))).unwrap();
        let (mut pb, mut sb) = (got.lines(), want.lines());
        loop {
            let (ps, ss) = (pb.next(), sb.next());
            let (ps, ss) = match (ps, ss) {
                (None, None) => break,
                (Some(p), Some(s)) => (p, s),
                _ => panic!("{fn_}: Different sizes"),
            };
            // Newer versions of the sng command line tool append an optional color name to the
            // RGB tuple; strip it.
            let mut ss = ss;
            if ss.contains("# rgb = (")
                && !ss.ends_with(')')
                && let Some(i) = ss.rfind(") ")
            {
                ss = &ss[..=i];
            }
            assert_eq!(ps, ss, "{fn_}: mismatch");
        }
    }
}

#[test]
fn reader_error() {
    let cases = [
        ("invalid-zlib.png", "zlib: invalid checksum"),
        ("invalid-crc32.png", "invalid checksum"),
        ("invalid-noend.png", "unexpected EOF"),
        ("invalid-trunc.png", "unexpected EOF"),
    ];
    for (file, want) in cases {
        let err = read_png(file).expect_err(file);
        assert!(err.to_string().contains(want), "{file}: {err}, want {want}");
    }
    // The classes the parity oracle sees: truncation is io.ErrUnexpectedEOF, the rest untyped.
    assert_eq!(
        read_png("invalid-trunc.png").unwrap_err().class(),
        "pdf/unexpected-eof"
    );
    assert_eq!(read_png("invalid-zlib.png").unwrap_err().class(), "error");
    assert_eq!(read_png("invalid-crc32.png").unwrap_err().class(), "error");
}

#[test]
fn paletted_decode_config() {
    for fn_ in FILENAMES_PALETTED {
        let data = fs::read(path(&format!("pngsuite/{fn_}.png"))).unwrap();
        let cfg = decode_config(&data, &Limits::default()).unwrap_or_else(|e| panic!("{fn_}: {e}"));
        let ColorModel::Paletted(pal) = cfg.model else {
            panic!("{fn_}: expected paletted color model");
        };
        assert!(!pal.is_empty(), "{fn_}: palette not initialized");
    }
}

#[test]
fn interlaced() {
    let a = read_png("gray-gradient.png").unwrap();
    let b = read_png("gray-gradient.interlaced.png").unwrap();
    assert_eq!(a, b, "decodings differ");
}

#[test]
fn incomplete_idat_on_row_boundary() {
    // An invalid 1x2 grayscale PNG image. The header is OK, but the zlib-compressed IDAT payload
    // contains two bytes "\x02\x00", which is only one row of data (the leading "\x02" is a row
    // filter).
    const IHDR: &[u8] =
        b"\x00\x00\x00\x0dIHDR\x00\x00\x00\x01\x00\x00\x00\x02\x08\x00\x00\x00\x00\xbc\xea\xe9\xfb";
    const IDAT: &[u8] = b"\x00\x00\x00\x0eIDAT\x78\x9c\x62\x62\x00\x04\x00\x00\xff\xff\x00\x06\x00\x03\xfa\xd0\x59\xae";
    const IEND: &[u8] = b"\x00\x00\x00\x00IEND\xae\x42\x60\x82";
    let err = decode(&cat(&[PNG_HEADER, IHDR, IDAT, IEND])).unwrap_err();
    assert_eq!(
        err,
        Error::Image("png: invalid format: not enough pixel data".into())
    );
}

#[test]
fn trailing_idat_chunks() {
    // A valid 1x1 PNG image containing color.Gray{255} and a trailing zero-length IDAT chunk
    // (see PNG specification section 12.9).
    const IHDR: &[u8] =
        b"\x00\x00\x00\x0dIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x00\x00\x00\x00\x3a\x7e\x9b\x55";
    const IDAT_WHITE: &[u8] = b"\x00\x00\x00\x0eIDAT\x78\x9c\x62\xfa\x0f\x08\x00\x00\xff\xff\x01\x05\x01\x02\x5a\xdd\x39\xcd";
    const IDAT_ZERO: &[u8] = b"\x00\x00\x00\x00IDAT\x35\xaf\x06\x1e";
    const IEND: &[u8] = b"\x00\x00\x00\x00IEND\xae\x42\x60\x82";
    let img = decode(&cat(&[PNG_HEADER, IHDR, IDAT_WHITE, IDAT_ZERO, IEND]))
        .expect("decoding valid image");
    assert_eq!(img.at(0, 0), Color::Gray(255));

    // Non-zero-length trailing IDAT chunks should be ignored (recoverable error). The following
    // chunk contains a single pixel with color.Gray{0}.
    const IDAT_BLACK: &[u8] = b"\x00\x00\x00\x0eIDAT\x78\x9c\x62\x62\x00\x04\x00\x00\xff\xff\x00\x06\x00\x03\xfa\xd0\x59\xae";
    let img = decode(&cat(&[PNG_HEADER, IHDR, IDAT_WHITE, IDAT_BLACK, IEND]))
        .expect("trailing IDAT not ignored");
    assert_ne!(
        img.at(0, 0),
        Color::Gray(0),
        "decoded image from trailing IDAT chunk"
    );
}

#[test]
fn multiple_trns_chunks() {
    // A valid 1x1 paletted PNG image with a 1-element palette containing
    // color.NRGBA{0xff, 0x00, 0x00, 0x7f}. Dropping the tRNS chunk makes that color's alpha 0xff
    // instead of 0x7f.
    const IHDR: &[u8] =
        b"\x00\x00\x00\x0dIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x03\x00\x00\x00\x28\xcb\x34\xbb";
    const PLTE: &[u8] = b"\x00\x00\x00\x03PLTE\xff\x00\x00\x19\xe2\x09\x37";
    const TRNS: &[u8] = b"\x00\x00\x00\x01tRNS\x7f\x80\x5c\xb4\xcb";
    const IDAT: &[u8] = b"\x00\x00\x00\x0eIDAT\x78\x9c\x62\x62\x00\x04\x00\x00\xff\xff\x00\x06\x00\x03\xfa\xd0\x59\xae";
    const IEND: &[u8] = b"\x00\x00\x00\x00IEND\xae\x42\x60\x82";
    for i in 0..4 {
        let mut b = cat(&[PNG_HEADER, IHDR, PLTE]);
        for _ in 0..i {
            b.extend_from_slice(TRNS);
        }
        b.extend_from_slice(IDAT);
        b.extend_from_slice(IEND);

        let m = decode(&b);
        let want = match i {
            0 => Color::Rgba {
                r: 0xff,
                g: 0,
                b: 0,
                a: 0xff,
            },
            1 => Color::Nrgba {
                r: 0xff,
                g: 0,
                b: 0,
                a: 0x7f,
            },
            _ => {
                assert!(m.is_err(), "{i} tRNS chunks: got nil error, want non-nil");
                continue;
            }
        };
        let m = m.unwrap_or_else(|e| panic!("{i} tRNS chunks: {e}"));
        assert_eq!(m.at(0, 0), want, "{i} tRNS chunks");
    }
}

#[test]
fn unknown_chunk_length_underflow() {
    assert!(
        decode(&UNKNOWN_CHUNK).is_err(),
        "Didn't fail reading an unknown chunk with length 0xffffffff"
    );
}

#[test]
fn paletted8_out_of_range_pixel() {
    // IDAT contains a reference to a palette index that does not exist in the file.
    let img = read_png("invalid-palette.png").expect("decoding invalid-palette.png");
    // The palette is extended with opaque black.
    assert_eq!(
        img.at(15, 15),
        Color::Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 0xff
        }
    );
}

#[test]
fn gray8_transparent() {
    // These bytes come from https://golang.org/issues/19553
    let m = decode(&GRAY8_TRANSPARENT).expect("Decode");
    const HEX: &[u8] = b"0123456789abcdef";
    let mut got = String::new();
    let bounds = m.bounds();
    for y in bounds.y0..bounds.y1 {
        for x in bounds.x0..bounds.x1 {
            let (r, _, _, a) = m.at(x, y).rgba();
            if a != 0 {
                got.push(HEX[(0x0f & (r >> 12)) as usize] as char);
                got.push(HEX[(0x0f & (r >> 8)) as usize] as char);
                got.push(' ');
            } else {
                got.push_str(".. ");
            }
        }
        got.push('\n');
    }

    let want = "\
.. .. .. ce bd bd bd bd bd bd bd bd bd bd e6 \n\
.. .. .. 7b 84 94 94 94 94 94 94 94 94 6b bd \n\
.. .. .. 7b d6 .. .. .. .. .. .. .. .. 8c bd \n\
.. .. .. 7b d6 .. .. .. .. .. .. .. .. 8c bd \n\
.. .. .. 7b d6 .. .. .. .. .. .. .. .. 8c bd \n\
e6 bd bd 7b a5 bd bd f7 .. .. .. .. .. 8c bd \n\
bd 6b 94 94 94 94 5a ef .. .. .. .. .. 8c bd \n\
bd 8c .. .. .. .. 63 ad ad ad ad ad ad 73 bd \n\
bd 8c .. .. .. .. 63 9c 9c 9c 9c 9c 9c 9c de \n\
bd 6b 94 94 94 94 5a ef .. .. .. .. .. .. .. \n\
e6 b5 b5 b5 b5 b5 b5 f7 .. .. .. .. .. .. .. \n";
    assert_eq!(got, want);
}

#[test]
fn dimension_overflow() {
    // (source, unsupported config, width, height). The first encodes a 2147483646 x 2147483646
    // NRGBA image: width x height does not overflow an int64, but width x height x 8 does. The
    // others are from https://golang.org/issues/38435.
    let cases: [(&[u8], bool, usize, usize); 4] = [
        (&OVERFLOW_0[..], true, 0x7fff_fffe, 0x7fff_fffe),
        (&OVERFLOW_1[..], false, 0x0000_b504, 0x0000_b504),
        (&OVERFLOW_2[..], false, 0x0400_0000, 1),
        (&OVERFLOW_3[..], false, 0x0800_0000, 1),
    ];
    // A small budget keeps Decode from allocating the (up to 2 GiB) pixel buffers.
    let limits = Limits {
        max_alloc: 1 << 20,
        ..Limits::default()
    };
    for (i, (src, unsupported_config, width, height)) in cases.into_iter().enumerate() {
        let cfg = decode_config(src, &limits);
        if unsupported_config {
            assert_eq!(
                cfg.unwrap_err(),
                Error::Image("png: unsupported feature: dimension overflow".into()),
                "i={i}"
            );
            continue;
        }
        let cfg = cfg.unwrap_or_else(|e| panic!("i={i}: DecodeConfig: {e}"));
        assert_eq!((cfg.width, cfg.height), (width, height), "i={i}");

        // Even if we don't panic, these aren't valid PNG images.
        let err = png_read::decode(src, &limits, &AtomicBool::new(false));
        assert!(err.is_err(), "i={i}: Decode: got nil error, want non-nil");
    }
}

#[test]
fn decode_paletted_with_transparency() {
    // These bytes come from https://go.dev/issue/54325. Both Decode and DecodeConfig should pick
    // up that the first palette entry's alpha is zero.
    let cfg = decode_config(&PALETTED_TRNS, &Limits::default()).expect("DecodeConfig");
    let ColorModel::Paletted(pal) = cfg.model else {
        panic!("not paletted")
    };
    assert_eq!(pal[0].rgba().3, 0, "DecodeConfig");

    let img = decode(&PALETTED_TRNS).expect("Decode");
    let Image::Paletted(p) = img else {
        panic!("not paletted")
    };
    assert_eq!(p.palette[0].rgba().3, 0, "Decode");
}

// Beyond Go's tests: the limits and the byte source.

#[test]
fn budget_and_cancel() {
    let data = fs::read(path("pngsuite/basn6a16.png")).unwrap();
    let tiny = Limits {
        max_alloc: 64,
        ..Limits::default()
    };
    let e = png_read::decode(&data, &tiny, &AtomicBool::new(false)).unwrap_err();
    assert_eq!(e.class(), "limit/alloc");

    let e = png_read::decode(&data, &Limits::default(), &AtomicBool::new(true)).unwrap_err();
    assert_eq!(e, Error::Cancelled);
    assert!(png_read::decode(&data, &Limits::default(), &AtomicBool::new(false)).is_ok());
}

#[test]
fn idat_length_one_too_long_is_an_error() {
    // The parity oracle's mutation: the first IDAT length grows by one, so the chunk takes the
    // first byte of its checksum. Go's bufio swallows the byte, and then the checksum is wrong.
    let mut data = fs::read(path("pngsuite/basn0g08.png")).unwrap();
    let at = data.windows(4).position(|w| w == b"IDAT").unwrap() - 4;
    let n = u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) + 1;
    data[at..at + 4].copy_from_slice(&n.to_be_bytes());
    let e = decode(&data).unwrap_err();
    assert_eq!(e.class(), "error");
}

#[test]
fn truncated_everywhere_is_an_error_and_never_a_panic() {
    let data = fs::read(path("pngsuite/basn3p04-31i.png")).unwrap();
    for n in 0..data.len() {
        assert!(decode(&data[..n]).is_err(), "prefix of {n} bytes");
        let _ = decode_config(&data[..n], &Limits::default());
    }
}
