//! Tests of the layout planner and the file assembler. Go has none of its own for these two
//! files (the hint vectors of `hints_test.go` are in `hints.rs`; the assembled file is checked
//! against qpdf in byblos's root package, which needs a PDF reader).

use super::*;
use crate::limits::Limits;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

/// Catalog 1 -> page tree 2 -> pages 3 and 4, with a content stream each (5 and 6).
fn two_page_graph() -> Graph {
    let mut refs = BTreeMap::new();
    refs.insert(1, vec![2]);
    refs.insert(2, vec![3, 4]);
    refs.insert(3, vec![5]);
    refs.insert(4, vec![6]);
    refs.insert(5, vec![]);
    refs.insert(6, vec![]);
    Graph {
        refs,
        pages: vec![3, 4],
        page_tree: vec![2],
        catalog: 1,
        ..Graph::default()
    }
}

#[test]
fn plan_layout_numbers_the_two_groups() {
    let cancel = AtomicBool::new(false);
    let p = plan_layout(&two_page_graph(), &Limits::default(), &cancel).unwrap();
    // Parts 7, 8 and 9 take 1..k: page 2's objects (4, 6), then the page tree (2).
    assert_eq!(p.part7, vec![vec![1, 2]]);
    assert!(p.part8.is_empty());
    assert_eq!(p.part9, vec![3]);
    // Then the linearization dictionary, part 4 (the catalog), the hint stream, part 6.
    assert_eq!(p.lin_dict, 4);
    assert_eq!(p.part4, vec![5]);
    assert_eq!(p.catalog, 5);
    assert_eq!(p.hint, 6);
    assert_eq!(p.part6, vec![7, 8]);
    assert_eq!(p.size, 9);
    assert_eq!(p.n_pages, 2);
    assert_eq!(p.page_shared, vec![Vec::<i64>::new(), Vec::new()]);
    assert_eq!(p.renumber.len(), 6);
    assert_eq!(p.renumber.get(&3), Some(&7));
}

#[test]
fn plan_layout_refuses_bad_graphs() {
    let cancel = AtomicBool::new(false);
    let lim = Limits::default();
    assert!(plan_layout(&Graph::default(), &lim, &cancel).is_err());
    let mut g = two_page_graph();
    g.catalog = 0;
    assert!(plan_layout(&g, &lim, &cancel).is_err());
    let mut g = two_page_graph();
    g.pages.push(99);
    assert!(plan_layout(&g, &lim, &cancel).is_err());
    let mut g = two_page_graph();
    g.catalog = 99;
    assert!(plan_layout(&g, &lim, &cancel).is_err());
}

#[test]
fn write_is_self_consistent() {
    let cancel = AtomicBool::new(false);
    let lim = Limits::default();
    let p = plan_layout(&two_page_graph(), &lim, &cancel).unwrap();
    let mut bodies = BTreeMap::new();
    for n in [1, 2, 3, 5, 7, 8] {
        bodies.insert(n, b"<< /Type /Test >>".to_vec());
    }
    let meta = Meta {
        version: "1.5".to_string(),
        id: [vec![1, 2, 3], vec![]],
    };
    let mut out: Vec<u8> = Vec::new();
    write(&mut out, &p, &bodies, &meta, &lim, &cancel).unwrap();

    let s = String::from_utf8_lossy(&out).into_owned();
    assert!(out.starts_with(b"%PDF-1.5\n"));
    assert!(out.ends_with(b"%%EOF\n"));
    // /L is the length of the finished file.
    assert!(s.contains(&format!("/Linearized 1 /L {} /H [ ", out.len())));
    assert!(s.contains("/O 7 "));
    assert!(s.contains("/N 2 "));
    // The front cross-reference section is one subsection over objects 4..=8.
    assert!(s.contains("xref\n4 5\n"));
    assert!(s.contains("/ID [<010203><010203>]"));

    // Without a body, the assembler says which object it misses.
    bodies.remove(&2);
    let mut sink: Vec<u8> = Vec::new();
    assert!(write(&mut sink, &p, &bodies, &meta, &lim, &cancel).is_err());
    assert!(sink.is_empty());
}

#[test]
fn write_refuses_an_empty_first_page_section() {
    let cancel = AtomicBool::new(false);
    let mut sink: Vec<u8> = Vec::new();
    let r = write(
        &mut sink,
        &Plan::default(),
        &BTreeMap::new(),
        &Meta::default(),
        &Limits::default(),
        &cancel,
    );
    assert!(r.is_err());
}
