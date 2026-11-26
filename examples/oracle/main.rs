//! The Rust side of the parity oracle (docs/parity.md): `oracle [-threads N] <in-dir> <out-dir>`
//! runs every area on every input file and writes the same output files as the Go oracle.

mod build;
mod common;
mod content;
mod image;
mod jbig2;
mod pdf;
mod render;
mod sha256;

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("oracle: {e}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> std::io::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut threads = 0;
    if args.first().map(String::as_str) == Some("-threads") {
        let bad = || std::io::Error::other("usage: oracle [-threads N] <in-dir> <out-dir>");
        if args.len() < 2 {
            return Err(bad());
        }
        threads = args[1].parse().map_err(|_| bad())?;
        args.drain(..2);
    }
    let [in_dir, out_dir] = args.as_slice() else {
        return Err(std::io::Error::other(
            "usage: oracle [-threads N] <in-dir> <out-dir>",
        ));
    };
    let out = Path::new(out_dir);
    common::set_in_dir(Path::new(in_dir));
    std::fs::create_dir_all(out)?;
    let mut names = Vec::new();
    for entry in std::fs::read_dir(in_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    for name in &names {
        let data = std::fs::read(Path::new(in_dir).join(name))?;
        image::run(name, &data, threads, out)?;
        jbig2::run(name, &data, threads, out)?;
        content::run(name, &data, threads, out)?;
        render::run(name, &data, threads, out)?;
        build::run(name, &data, threads, out)?;
        pdf::run(name, &data, threads, out)?;
    }
    Ok(())
}
