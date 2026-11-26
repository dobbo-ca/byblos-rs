//! Port of byblos `internal/linearize/layout.go`: the Annex F object partition and renumbering.
//!
//! F.3.1 divides a linearized file into eleven parts and constrains both the physical order of
//! the objects and their NUMBERING. Byblos emits parts 1-9 and 11; part 10, the overflow hint
//! stream, is never written (qpdf does not write one either, and it has not been necessary
//! since PDF 1.4).
//!
//! ```text
//! part 1   header
//! part 2   linearization parameter dictionary
//! part 3   first-page cross-reference section and trailer
//! part 4   catalog and other document-level objects
//! part 5   primary hint stream
//! part 6   first-page section
//! part 7   remaining pages, in page order
//! part 8   objects shared between later pages but not used by the first page
//! part 9   everything else: the page tree, the info dictionary, thumbnails
//! part 11  main cross-reference section and trailer
//! ```
//!
//! The numbering is two contiguous groups: parts 7, 8 and 9 take 1..k, and parts 2, 4, 5 and 6
//! take the numbers after that. Part 6 being one ascending run starting at the first page's
//! object number is what lets the front cross-reference section be a single subsection and the
//! page-offset hint table address a page by "the next N object numbers".
//!
//! Go iterates maps and sorts each part afterwards (layout.go:184-194); the Rust maps are
//! ordered, and the parts are sorted the same way, so the plan is identical.

use super::malformed;
use crate::error::Result;
use crate::limits::{Limits, check_cancel};
use pdf::Held;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;

/// What one entry of a closure set costs against `max_alloc`.
const SET_ENTRY_BYTES: u64 = 64;

/// `Graph`: a document's object graph, with no PDF parser attached. Every object number in it
/// is the ORIGINAL number from the input document.
///
/// `refs` holds the outgoing references of every object that must survive, so its key set is
/// also the set of objects to be written. A page dictionary's `/Parent` and `/Thumb` are
/// deliberately absent from its entry: `/Parent` points back at the page tree, which Annex
/// F.3.10 puts in part 9, and a thumbnail is not needed to display the page. Both would
/// otherwise drag part-9 material into the first-page section. They stay in the key set because
/// they are still reachable and still get written; they just do not steer the partition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Graph {
    pub refs: BTreeMap<i64, Vec<i64>>,
    /// Page object numbers, in page order.
    pub pages: Vec<i64>,
    /// Every `/Pages` node, root first.
    pub page_tree: Vec<i64>,
    pub catalog: i64,
    /// 0 when the document has none.
    pub info: i64,
    /// The direct targets of the catalog keys a reader consults before it can display
    /// anything: `/ViewerPreferences`, `/PageMode`, `/Threads`, `/OpenAction` and `/AcroForm`.
    /// They belong to part 4.
    pub open_doc: Vec<i64>,
    /// The catalog's `/Outlines` target (0 for none), and `use_outlines` records whether
    /// `/PageMode` is `/UseOutlines`. An outline tree that will be shown immediately belongs to
    /// the first-page section; one that will not belongs to part 9 (F.3.8).
    pub outlines: i64,
    pub use_outlines: bool,
    /// The direct targets of every remaining catalog key, plus the info dictionary. An object
    /// reachable from one of these is document-level material that no single page owns, so it
    /// may not be filed under a page even when exactly one page also refers to it.
    pub other: Vec<i64>,
}

/// `Plan`: the layout decision. Every object number in it is the NEW number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Maps original object number to new, over exactly `Graph::refs`'s key set.
    pub renumber: BTreeMap<i64, i64>,

    /// Catalog first.
    pub part4: Vec<i64>,
    /// `part6[0]` is the first page's dictionary, i.e. `/O`.
    pub part6: Vec<i64>,
    /// One group per page 2..N; each group's page dictionary first.
    pub part7: Vec<Vec<i64>>,
    pub part8: Vec<i64>,
    pub part9: Vec<i64>,

    /// `page_shared[i]` holds, for page `i`, the indices into the shared-object hint table
    /// (`part6` followed by `part8`) of the shared objects that page uses. `page_shared[0]` is
    /// always empty: Table F.4 defines the first page's shared objects implicitly.
    pub page_shared: Vec<Vec<i64>>,

    /// The outline tree's objects in placement order, the `/Outlines` root first. They sit at
    /// the end of part 6 or the end of part 9, and they are also members of whichever of those
    /// they went into, so this is an index, not a fourth part. It is empty when the document
    /// has no outline tree, and non-empty means the primary hint stream must carry an outline
    /// hint table: the reference implementation computes one whether the outlines went to part
    /// 6 or part 9, and warns "incorrect object count in outline hint table" when the file does
    /// not have one.
    pub outlines: Vec<i64>,

    /// The linearization parameter dictionary's object number.
    pub lin_dict: i64,
    /// The primary hint stream's object number.
    pub hint: i64,
    pub catalog: i64,
    /// 0 when the document has none.
    pub info: i64,
    /// Trailer `/Size` for the whole file: every object plus object 0.
    pub size: i64,
    pub n_pages: i64,
}

/// `PlanLayout`: partitions `g` and assigns new object numbers.
///
/// The closure sets it builds (one per page) are charged to `limits.max_alloc`; `cancel` is
/// checked once per page.
///
/// # Errors
/// [`crate::Error::Malformed`] for no pages, no catalog, a page or the catalog missing from
/// the graph, or a partition that loses an object; [`crate::Error::Alloc`];
/// [`crate::Error::Cancelled`].
pub fn plan_layout(g: &Graph, limits: &Limits, cancel: &AtomicBool) -> Result<Plan> {
    if g.pages.is_empty() {
        return Err(malformed("linearize: the document has no pages"));
    }
    if g.catalog == 0 {
        return Err(malformed("linearize: the document has no catalog"));
    }
    for n in &g.pages {
        if !g.refs.contains_key(n) {
            return Err(malformed(format!(
                "linearize: page object {n} is not in the graph"
            )));
        }
    }
    if !g.refs.contains_key(&g.catalog) {
        return Err(malformed(format!(
            "linearize: catalog object {} is not in the graph",
            g.catalog
        )));
    }

    let mem = limits.mem();
    let mut held = mem.charge(0)?;

    // A closure never walks into a page dictionary or a page-tree node it did not start from.
    // Without that rule an outline item's /Dest, or a /Next link, pulls a later page's objects
    // into the first-page section, and it is how the reference implementation behaves (qpdf's
    // updateObjectMaps returns as soon as it reaches a /Page node that is not the top of the
    // walk).
    let barrier: BTreeSet<i64> = g.pages.iter().chain(&g.page_tree).copied().collect();

    check_cancel(cancel)?;
    let first_page = g.closure(g.pages[0], &barrier, &mut held)?;
    let mut other_pages: Vec<BTreeSet<i64>> = vec![BTreeSet::new()];
    // Object -> how many of pages 2..N reach it.
    let mut page_count: BTreeMap<i64, usize> = BTreeMap::new();
    for &page in g.pages.iter().skip(1) {
        check_cancel(cancel)?;
        let set = g.closure(page, &barrier, &mut held)?;
        for &n in &set {
            let c = page_count.entry(n).or_insert(0);
            if *c == 0 {
                held.grow_by(SET_ENTRY_BYTES)?;
            }
            *c += 1;
        }
        other_pages.push(set);
    }
    let open_doc = g.closure_of(&g.open_doc, &barrier, &mut held)?;
    let others = g.closure_of(&g.other, &barrier, &mut held)?;
    let outlines = if g.outlines != 0 {
        g.closure(g.outlines, &barrier, &mut held)?
    } else {
        BTreeSet::new()
    };
    // The renumbering map and the parts hold one entry per object.
    held.grow_by((g.refs.len() as u64).saturating_mul(SET_ENTRY_BYTES))?;

    let mut p = Plan {
        n_pages: g.pages.len() as i64,
        ..Plan::default()
    };
    let mut part7: Vec<Vec<i64>> = vec![Vec::new(); g.pages.len() - 1];
    let mut part4 = Vec::new();
    let mut part6 = Vec::new();
    let mut part8 = Vec::new();
    let mut part9 = Vec::new();
    let mut outline_group = Vec::new();

    // The cascade, first match wins. It mirrors the reference implementation's
    // (QPDF_linearization.cc, calculateLinearizationData): an object reachable from the first
    // page goes to part 6 whether or not later pages also use it, but an object reachable from
    // exactly one LATER page is only that page's private property if no document-level key
    // reaches it too.
    for (i, &n) in g.refs.keys().enumerate() {
        if i % 4096 == 0 {
            check_cancel(cancel)?;
        }
        let count = page_count.get(&n).copied().unwrap_or(0);
        if n == g.catalog {
            part4.push(n);
        } else if outlines.contains(&n) {
            outline_group.push(n);
        } else if open_doc.contains(&n) {
            part4.push(n);
        } else if first_page.contains(&n) {
            part6.push(n);
        } else if count == 1 && !others.contains(&n) {
            for i in 1..g.pages.len() {
                if other_pages.get(i).is_some_and(|s| s.contains(&n)) {
                    if let Some(group) = part7.get_mut(i - 1) {
                        group.push(n);
                    }
                    break;
                }
            }
        } else if count > 1 {
            part8.push(n);
        } else {
            part9.push(n);
        }
    }

    // Ordering inside each part. The leading object of each is fixed by Annex F; everything
    // else is sorted by original number so the output is deterministic for a given input.
    let part4 = lead(g.catalog, part4);
    let mut part6 = lead(g.pages[0], part6);
    for (i, group) in part7.iter_mut().enumerate() {
        *group = lead(g.pages[i + 1], std::mem::take(group));
    }
    part8.sort_unstable();
    let mut part9 = lead_all(&g.page_tree, part9);

    // F.3.8: the outline tree goes in the first-page section when /PageMode is /UseOutlines,
    // because the reader has to draw it before it can show anything, and in part 9 otherwise.
    // Either way it is placed as one contiguous run with the /Outlines root at its head, which
    // is what makes the outline hint table's single (object, offset, count, length) tuple
    // describe it.
    let outline_group = lead(g.outlines, outline_group);
    if g.use_outlines {
        part6.extend_from_slice(&outline_group);
    } else {
        part9.extend_from_slice(&outline_group);
    }

    // Numbering, F.3.1. Parts 7, 8 and 9 take 1..k in emission order; the first group follows,
    // in the order the file physically holds it apart from the hint stream, which the
    // reference implementation also numbers before part 6.
    let mut renumber: BTreeMap<i64, i64> = BTreeMap::new();
    let mut next = 1i64;
    for grp in &part7 {
        assign(&mut renumber, &mut next, grp);
    }
    assign(&mut renumber, &mut next, &part8);
    assign(&mut renumber, &mut next, &part9);
    p.lin_dict = next;
    next += 1;
    assign(&mut renumber, &mut next, &part4);
    p.hint = next;
    next += 1;
    assign(&mut renumber, &mut next, &part6);
    p.size = next;

    if renumber.len() != g.refs.len() {
        return Err(malformed(format!(
            "linearize: partitioned {} objects out of {}",
            renumber.len(),
            g.refs.len()
        )));
    }

    // The shared-object hint table is part 6 followed by part 8, and a page's shared
    // identifiers are indices into that concatenation.
    let mut shared_idx: BTreeMap<i64, i64> = BTreeMap::new();
    for (i, &n) in part6.iter().enumerate() {
        shared_idx.insert(n, i as i64);
    }
    for (i, &n) in part8.iter().enumerate() {
        shared_idx.insert(n, (part6.len() + i) as i64);
    }
    p.page_shared = vec![Vec::new()];
    for set in other_pages.iter().skip(1) {
        let mut ids: Vec<i64> = set
            .iter()
            .filter_map(|n| shared_idx.get(n).copied())
            .collect();
        ids.sort_unstable();
        p.page_shared.push(ids);
    }

    p.catalog = renumber.get(&g.catalog).copied().unwrap_or(0);
    if g.info != 0 {
        p.info = renumber.get(&g.info).copied().unwrap_or(0);
    }
    // Renumber the parts so callers never see original numbers.
    p.part4 = renumber_all(&renumber, &part4);
    p.part6 = renumber_all(&renumber, &part6);
    p.part7 = part7.iter().map(|g| renumber_all(&renumber, g)).collect();
    p.part8 = renumber_all(&renumber, &part8);
    p.part9 = renumber_all(&renumber, &part9);
    p.outlines = renumber_all(&renumber, &outline_group);
    p.renumber = renumber;
    Ok(p)
}

impl Graph {
    /// `closure`: walks `refs` from `start`, refusing to enter any barrier object other than
    /// `start` itself.
    fn closure(
        &self,
        start: i64,
        barrier: &BTreeSet<i64>,
        held: &mut Held,
    ) -> Result<BTreeSet<i64>> {
        let mut seen = BTreeSet::new();
        if !self.refs.contains_key(&start) {
            return Ok(seen);
        }
        let mut stack = vec![start];
        while let Some(n) = stack.pop() {
            if seen.contains(&n) {
                continue;
            }
            let Some(out) = self.refs.get(&n) else {
                continue;
            };
            if n != start && barrier.contains(&n) {
                continue;
            }
            held.grow_by(SET_ENTRY_BYTES)?;
            seen.insert(n);
            stack.extend_from_slice(out);
        }
        Ok(seen)
    }

    /// `closureOf`: `closure` over several seeds, with no exemption: a seed that is itself a
    /// page or a page-tree node is not entered.
    fn closure_of(
        &self,
        seeds: &[i64],
        barrier: &BTreeSet<i64>,
        held: &mut Held,
    ) -> Result<BTreeSet<i64>> {
        let mut out = BTreeSet::new();
        for &s in seeds {
            if barrier.contains(&s) {
                continue;
            }
            out.extend(self.closure(s, barrier, held)?);
        }
        Ok(out)
    }
}

/// Numbers `ns` from `*next` on.
fn assign(renumber: &mut BTreeMap<i64, i64>, next: &mut i64, ns: &[i64]) {
    for &n in ns {
        renumber.insert(n, *next);
        *next += 1;
    }
}

/// `lead`: sorts `ns` and moves `first` to the front, if it is there at all.
fn lead(first: i64, mut ns: Vec<i64>) -> Vec<i64> {
    ns.sort_unstable();
    if let Some(i) = ns.iter().position(|&n| n == first)
        && i > 0
    {
        ns.remove(i);
        ns.insert(0, first);
    }
    ns
}

/// `leadAll`: sorts `ns` and moves every member of `firsts`, in order, to the front.
fn lead_all(firsts: &[i64], mut ns: Vec<i64>) -> Vec<i64> {
    ns.sort_unstable();
    let present: BTreeSet<i64> = ns.iter().copied().collect();
    let mut taken: BTreeSet<i64> = BTreeSet::new();
    let mut head = Vec::new();
    for &f in firsts {
        if present.contains(&f) && taken.insert(f) {
            head.push(f);
        }
    }
    ns.retain(|n| !taken.contains(n));
    head.extend(ns);
    head
}

/// `renumberAll`: an object with no new number maps to 0, as a Go map lookup does.
fn renumber_all(m: &BTreeMap<i64, i64>, ns: &[i64]) -> Vec<i64> {
    ns.iter().map(|n| m.get(n).copied().unwrap_or(0)).collect()
}
