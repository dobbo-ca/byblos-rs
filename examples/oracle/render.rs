//! The `render` operations of the parity oracle. A wave fills `run` in, following docs/parity.md
//! (the operations, file names and formats of the Go oracle).

use std::path::Path;

/// Runs this area's operations on one input and writes `<name>.<op>.out` or `.err` files to `out`.
pub fn run(_name: &str, _data: &[u8], _threads: usize, _out: &Path) -> std::io::Result<()> {
    Ok(())
}
