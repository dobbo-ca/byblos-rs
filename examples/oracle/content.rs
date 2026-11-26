//! The `content` operations of the parity oracle (docs/parity.md, "Operations"): `lex`, `walk`
//! and `walk-tight` on `.content` inputs. Mirrors `opLex`, `mockEnv` and `opWalk` of the Go
//! oracle's `ops_content.go`.

use crate::common::{Dw, base_lim, emit, ext_of, hex, quote, tight_lim};
use byblos::content::{BBox, Env, Lexer, Matrix, Scan, Stream, XObject, walk};
use byblos::{Error, Limits};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Bounds the token dump of `lex`.
const MAX_TOKENS: usize = 100_000;

/// `nameID`: FNV-1a-32 of the name, mod 1000, plus 1.
fn name_id(name: &[u8]) -> i64 {
    let mut h: u32 = 0x811c9dc5;
    for &b in name {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x01000193);
    }
    i64::from(h % 1000) + 1
}

/// `mockEnv`: the fixed Env of `walk` (docs/parity.md, "The mock env of walk").
struct MockEnv;

impl Env for MockEnv {
    fn xobject(&self, scope: i64, name: &[u8]) -> Option<XObject> {
        if name.starts_with(b"Im") {
            return Some(XObject {
                image: true,
                id: name_id(name),
                ..XObject::default()
            });
        }
        if name.starts_with(b"Fm") {
            let mut body = String::from(
                "q 2 0 0 2 5 5 cm /Im1 Do Q BT /F1 10 Tf (nested) Tj ET 0 0 50 50 re f\n",
            );
            match name {
                b"Fm1" => body.push_str("/Fm0 Do\n"),
                b"Fm2" => body.push_str("/Fm2 Do\n"),
                _ => {}
            }
            return Some(XObject {
                image: false,
                id: name_id(name),
                content: Arc::new(body.into_bytes()),
                matrix: Matrix([1.0, 0.0, 0.0, 1.0, 10.0, 20.0]),
                scope: scope + 1,
                bbox: if name.ends_with(b"x") {
                    None
                } else {
                    Some(BBox {
                        llx: 0.0,
                        lly: 0.0,
                        urx: 300.0,
                        ury: 400.0,
                    })
                },
            });
        }
        None
    }

    fn ext_gstate_opaque(&self, _scope: i64, name: &[u8]) -> bool {
        !name.ends_with(b"1")
    }

    fn font(&self, _scope: i64, name: &[u8]) -> Option<i64> {
        (name != b"F9").then(|| name_id(name))
    }
}

/// `opLex`: the tokens of the lexer, one per line: `<kind> <num as %016x> <text hex> <alias 0|1>`,
/// then `eof`, or `error <quoted message>` at the first error, or `truncated` after
/// `MAX_TOKENS` tokens.
fn lex(data: &[u8]) -> Vec<u8> {
    let mut o = String::new();
    let mut lx = Lexer::new(data);
    for i in 0.. {
        if i == MAX_TOKENS {
            o.push_str("truncated\n");
            break;
        }
        match lx.next_token() {
            Ok(None) => {
                o.push_str("eof\n");
                break;
            }
            Err(e) => {
                let msg = match e {
                    Error::Malformed(s) => s,
                    e => e.to_string(),
                };
                o.push_str(&format!("error {}\n", quote(msg.as_bytes())));
                break;
            }
            Ok(Some(t)) => {
                let hex = hex(&t.text);
                o.push_str(&format!(
                    "{} {:016x} {} {}\n",
                    t.kind as u8,
                    t.num.to_bits(),
                    hex,
                    u8::from(t.alias)
                ));
            }
        }
    }
    o.into_bytes()
}

fn dump_box(d: &mut Dw, k: &str, b: Option<&BBox>) {
    match b {
        None => d.nil(k),
        Some(b) => d.fs(k, &[b.llx, b.lly, b.urx, b.ury]),
    }
}

fn dump_color(d: &mut Dw, k: &str, c: &byblos::content::Color) {
    d.s(&format!("{k}.space"), &c.space);
    d.fs(&format!("{k}.comps"), &c.comps);
}

fn dump_placements(d: &mut Dw, k: &str, ps: &[byblos::content::Placement]) {
    d.n(k, ps.len());
    for (i, p) in ps.iter().enumerate() {
        let pk = format!("{k}.{i}.");
        d.s(&format!("{pk}name"), &p.name);
        d.i(&format!("{pk}id"), p.id);
        d.fs(&format!("{pk}ctm"), &p.ctm.0);
        dump_box(d, &format!("{pk}box"), Some(&p.bbox));
        d.b2(&format!("{pk}inline"), p.inline);
        dump_box(d, &format!("{pk}clip"), p.clip.as_ref());
        d.b2(&format!("{pk}opaque"), p.opaque);
        d.i(&format!("{pk}index"), p.index);
    }
}

fn dump_scan(s: &Scan) -> Vec<u8> {
    let mut d = Dw::default();
    dump_placements(&mut d, "images", &s.images);
    dump_placements(&mut d, "inline_images", &s.inline_images);
    d.i("text_chars", s.text_chars as i64);
    d.i("text_ops", s.text_ops as i64);
    d.i("inked_text_ops", s.inked_text_ops as i64);
    d.n("text_shows", s.text_shows.len());
    for (i, t) in s.text_shows.iter().enumerate() {
        let k = format!("text_shows.{i}.");
        d.hex(&format!("{k}raw"), t.raw.as_bytes());
        d.s(&format!("{k}font"), &t.font);
        d.i(&format!("{k}font_id"), t.font_id);
        d.f(&format!("{k}size"), t.size);
        d.f(&format!("{k}char_spacing"), t.char_spacing);
        d.f(&format!("{k}word_spacing"), t.word_spacing);
        d.f(&format!("{k}hscale"), t.hscale);
        d.f(&format!("{k}rise"), t.rise);
        d.i(&format!("{k}mode"), t.mode);
        d.fs(&format!("{k}trm"), &t.trm.0);
        d.i(&format!("{k}index"), t.index);
    }
    d.n("paints", s.paints.len());
    for (i, p) in s.paints.iter().enumerate() {
        let k = format!("paints.{i}.");
        d.s(&format!("{k}op"), p.op);
        dump_box(&mut d, &format!("{k}box"), Some(&p.bbox));
        dump_color(&mut d, &format!("{k}fill"), &p.fill);
        dump_color(&mut d, &format!("{k}stroke"), &p.stroke);
        d.i(&format!("{k}index"), p.index);
        dump_box(&mut d, &format!("{k}clip"), p.clip.as_ref());
        d.fs(&format!("{k}ctm"), &p.ctm.0);
    }
    d.i("shading_ops", s.shading_ops as i64);
    d.i("inline_imgs", s.inline_imgs as i64);
    d.strs("unresolved", &s.unresolved);
    d.b
}

/// `opWalk`: the Scan of the walk with the mock env under `lim`, or the class of its error. The
/// copy of the stream is checked against `max_alloc` first.
fn walk_op(data: &[u8], lim: &Limits, cancel: &AtomicBool) -> Result<Vec<u8>, Error> {
    let max = lim.alloc_limit();
    if data.len() as u64 > max {
        return Err(Error::Alloc {
            got: data.len() as u64,
            max,
        });
    }
    let mut copy = Vec::new();
    copy.try_reserve_exact(data.len())?;
    copy.extend_from_slice(data);
    let src: Stream = Arc::new(copy);
    let (scan, res) = walk(cancel, &src, 0, &MockEnv, lim);
    res?;
    Ok(dump_scan(&scan))
}

/// Runs this area's operations on one input and writes `<name>.<op>.out` or `.err` files to `out`.
pub fn run(name: &str, data: &[u8], threads: usize, out: &Path) -> std::io::Result<()> {
    if ext_of(name) != "content" {
        return Ok(());
    }
    let cancel = AtomicBool::new(false);
    emit(out, name, "lex", || Ok(Some(lex(data))))?;
    for (sfx, lim) in [("", base_lim(threads)), ("-tight", tight_lim(threads))] {
        emit(out, name, &format!("walk{sfx}"), || {
            walk_op(data, &lim, &cancel).map(Some)
        })?;
    }
    Ok(())
}
