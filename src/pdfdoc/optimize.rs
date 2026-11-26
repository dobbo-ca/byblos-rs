//! The structural-optimize seam: port of `internal/pdfdoc/optimize.go` (byb-b5).
//!
//! [`optimize`] is pdf-go's own read, validate and optimize pass (it dedupes shared resources,
//! fixes references to free objects, and writes with object and cross-reference streams) followed
//! by [`write_deterministic`]. The size and provenance policy around "is the result actually
//! smaller" lives in the crate's `optimize`, not here.
//!
//! # Differences from Go
//!
//! * Go runs under `defaultConfig()`: the package's default caps, never the caller's `Limits`.
//!   That is kept: only `max_alloc` and `max_threads` of `lim` reach pdf-rs ([`default_pdf_config`]).
//! * The clock and random source of the fallback writer are the caller's [`WriteEnv`].

use super::deterministic::write_deterministic;
use super::linearize::pdf_err;
use crate::error::Result;
use crate::limits::{Limits, check_cancel};
use crate::linearize::wrap;
use pdf::{Cmd, Config, Mem, WriteEnv};
use std::sync::atomic::AtomicBool;

/// `defaultConfig()` (`pdfdoc.go:81`): pdf-rs under the package's default caps (what Go's
/// `limits.Limits{}` resolves to), with the per-call `max_alloc` and `max_threads` of `lim`.
pub(crate) fn default_pdf_config(lim: &Limits) -> Config {
    let defaults = Limits::default();
    Config {
        limits: pdf::Limits {
            max_alloc: lim.alloc_limit(),
            max_threads: lim.threads(),
            ..defaults.pdf_limits(defaults.objects())
        },
        cmd: Cmd::Validate,
    }
}

/// `Optimize`: runs pdf-go's structural optimize pass over `data` and returns the
/// deterministic serialization. It validates `data` first, so a malformed or non-PDF input is
/// reported here rather than producing corrupt output.
///
/// # Errors
/// The read, validate and optimize errors of pdf-rs, with the context
/// `byblos/pdfdoc: optimize`; the errors of [`write_deterministic`]; `Error::Alloc`,
/// `Error::Cancelled`.
pub fn optimize(data: &[u8], lim: &Limits, env: &WriteEnv, cancel: &AtomicBool) -> Result<Vec<u8>> {
    optimize_with_mem(data, lim, &lim.mem(), env, cancel)
}

/// [`optimize`] on the caller's ledger `mem`.
///
/// # Errors
/// As [`optimize`].
pub(crate) fn optimize_with_mem(
    data: &[u8],
    lim: &Limits,
    mem: &Mem,
    env: &WriteEnv,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    check_cancel(cancel)?;
    let mut ctx =
        pdf::api::read_validate_and_optimize_with_mem(data, &default_pdf_config(lim), mem, cancel)
            .map_err(|e| pdf_err(e, "byblos/pdfdoc: optimize"))?;
    write_deterministic(&mut ctx, env, cancel).map_err(|e| wrap(e, "byblos/pdfdoc: optimize"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_ignores_the_caller_caps_but_not_the_budget() {
        let lim = Limits {
            max_objects: 5,
            max_stream_bytes: 7,
            max_alloc: 1 << 20,
            max_threads: 1,
            ..Limits::default()
        };
        let cfg = default_pdf_config(&lim);
        let want = Limits::default().pdf_limits(Limits::default().objects());
        assert_eq!(cfg.limits.max_object_count, want.max_object_count);
        assert_eq!(cfg.limits.max_decode_bytes, want.max_decode_bytes);
        assert_eq!(cfg.limits.max_alloc, 1 << 20);
        assert_eq!(cfg.limits.max_threads, 1);
    }

    #[test]
    fn not_a_pdf_is_an_error_not_a_panic() {
        let cancel = AtomicBool::new(false);
        let r = optimize(
            b"not a pdf",
            &Limits::default(),
            &WriteEnv::parity(),
            &cancel,
        );
        assert!(r.is_err());
    }

    #[test]
    fn cancelled_before_it_starts() {
        let cancel = AtomicBool::new(true);
        let r = optimize(
            b"%PDF-1.4",
            &Limits::default(),
            &WriteEnv::parity(),
            &cancel,
        );
        assert_eq!(r, Err(crate::Error::Cancelled));
    }
}
