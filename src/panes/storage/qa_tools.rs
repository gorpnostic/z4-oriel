//! QA: the "get apps" catalog's filter / category / search keys, without installing, searching the package
//! manager, listing installed apps or scanning the disk. Everything that would reach the machine is pre-filled
//! (installed list, apps, drives) so no background query ever starts; installs only get as far as the question.

use super::catalog::{self, CATALOG, CATS, Env, Os};
use super::*;
use crate::testkit::Kit;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

fn pane(k: &Kit, os: Os) -> Storage {
    let via = if os == Os::Windows { "winget" } else { "pacman" };
    let mut p = Storage::with_env(Env { os, aur_helper: None, flatpak: false, npm: true }, via);
    p.started = true; // no cleanup scan
    p.out.waker = Some(crate::pane::Waker { id: 1, tx: k.tx.clone() });
    p.installed = Some(catalog::Installed::default()); // no `winget list`
    p.apps = Some(Ok(vec![])); // no installed-apps query (backtab from the catalog lands there)
    p.drives = Some(vec![]);
    p
}

/// Nothing that reaches outside the pane has started (a homepage "open" is recorded, not run, and isn't an install).
fn quiet(p: &Storage) {
    assert!(p.launched.iter().all(|l| l.starts_with("open: ")), "ran {:?}", p.launched);
    assert!(!p.searching && p.sgen == 0, "a package search started");
    assert!(!p.inst_loading, "an installed-list query started");
    assert!(!p.fstarted, "a folder scan started");
}

fn names(p: &Storage) -> Vec<&'static str> {
    p.order(View::Catalog).iter().map(|&i| CATALOG[i].name).collect()
}

fn matches(i: usize, q: &str) -> bool {
    let e = &CATALOG[i];
    format!("{} {} {}", e.name, e.desc, e.cat.label()).to_lowercase().contains(&q.to_lowercase())
}

#[test]
fn qa_storage_catalog_filter_keys() {
    let mut k = Kit::new();
    let mut p = pane(&k, Os::Windows);
    k.key(&mut p, KeyCode::Char('4'));
    assert_eq!(p.view, View::Catalog);
    let all = names(&p);
    assert!(!all.is_empty());
    k.key(&mut p, KeyCode::Char('/'));
    assert!(p.input);
    k.typ(&mut p, "vpn");
    let got = names(&p);
    assert!(got.contains(&"Mullvad VPN") && got.contains(&"Proton VPN"), "{got:?}");
    assert!(p.order(View::Catalog).iter().all(|&i| matches(i, "vpn")));
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains("Mullvad VPN"), "{s}");
    // case, spaces and digits in the box (digits type, they don't switch category)
    for _ in 0..3 {
        k.key(&mut p, KeyCode::Backspace);
    }
    assert_eq!(names(&p), all, "an empty box shows the category again");
    k.typ(&mut p, "  VPN ");
    assert_eq!(names(&p), got, "upper case / padding don't matter");
    k.key(&mut p, KeyCode::Esc);
    assert!(!p.input && p.cfilter.is_empty());
    k.key(&mut p, KeyCode::Char('/'));
    k.typ(&mut p, "7zip 2");
    assert_eq!(p.cfilter, "7zip 2");
    assert_eq!(p.cat, 0);
    // ↑↓ inside the box move the pick without clearing the filter
    k.key(&mut p, KeyCode::Esc);
    k.key(&mut p, KeyCode::Char('/'));
    k.typ(&mut p, "e");
    k.key(&mut p, KeyCode::Down);
    k.key(&mut p, KeyCode::Down);
    let second = p.order(View::Catalog)[2];
    assert_eq!(p.selected(), Some(second));
    assert_eq!(p.cfilter, "e");
    // enter with matches just closes the box (no search)
    k.key(&mut p, KeyCode::Enter);
    assert!(!p.input && p.cfilter == "e");
    // a filter nothing matches, left with tab (enter would search the package manager)
    k.key(&mut p, KeyCode::Char('/'));
    k.typ(&mut p, "zzzqqqxx ü 日本");
    assert!(p.order(View::Catalog).is_empty());
    let s = k.render(&mut p, 150, 44);
    k.key(&mut p, KeyCode::Tab);
    assert!(!p.input);
    for code in [KeyCode::Down, KeyCode::Up, KeyCode::End, KeyCode::Home, KeyCode::PageDown, KeyCode::Char('i'), KeyCode::Enter, KeyCode::Char('o')] {
        k.key(&mut p, code); // an empty list: nothing to pick, nothing to install
    }
    assert!(p.confirm.is_none(), "{s}");
    // changing category clears the filter
    k.key(&mut p, KeyCode::Right);
    assert!(p.cfilter.is_empty() && p.cat == 1);
    quiet(&p);
}

#[test]
fn qa_storage_catalog_category_keys() {
    let mut k = Kit::new();
    let mut p = pane(&k, Os::Windows);
    k.key(&mut p, KeyCode::Char('4'));
    let n = CATS.len();
    // every category lists only its own apps, and together they make "all"
    let mut total = 0;
    for c in 1..=n {
        p.set_cat(c);
        let o = p.order(View::Catalog);
        assert!(o.iter().all(|&i| CATALOG[i].cat == CATS[c - 1]), "category {c}");
        assert_eq!(o.len(), p.cat_count(c));
        total += o.len();
    }
    assert_eq!(total, p.cat_count(0));
    // → / l walk forward and wrap, ← / h back
    p.set_cat(0);
    for i in 1..=n + 1 {
        k.key(&mut p, if i % 2 == 0 { KeyCode::Right } else { KeyCode::Char('l') });
        assert_eq!(p.cat, i % (n + 1));
    }
    k.key(&mut p, KeyCode::Left);
    assert_eq!(p.cat, n);
    k.key(&mut p, KeyCode::Char('h'));
    assert_eq!(p.cat, n - 1);
    // digits: 1 = all, 2.. = categories, clamped
    for (c, want) in [('1', 0), ('2', 1), ('9', 8.min(n))] {
        k.key(&mut p, KeyCode::Char(c));
        assert_eq!(p.cat, want, "digit {c}");
    }
    // tab walks the categories then leaves for the search view (its box open, nothing searched)
    p.set_cat(0);
    for _ in 0..n {
        k.key(&mut p, KeyCode::Tab);
    }
    assert_eq!((p.view, p.cat), (View::Catalog, n));
    k.key(&mut p, KeyCode::Tab);
    assert_eq!(p.view, View::Install);
    assert!(p.input && p.found.is_none());
    k.key(&mut p, KeyCode::Esc);
    k.key(&mut p, KeyCode::BackTab);
    assert_eq!((p.view, p.cat), (View::Catalog, n));
    // backtab from "all" goes on to the installed apps view (pre-filled here, so nothing is queried)
    p.set_cat(0);
    k.key(&mut p, KeyCode::BackTab);
    assert_eq!(p.view, View::Apps);
    quiet(&p);
}

#[test]
fn qa_storage_catalog_s_opens_the_search_box_without_searching() {
    let mut k = Kit::new();
    let mut p = pane(&k, Os::Windows);
    k.key(&mut p, KeyCode::Char('4'));
    k.key(&mut p, KeyCode::Char('s')); // empty filter: just the box
    assert_eq!(p.view, View::Install);
    assert!(p.input && p.query.is_empty());
    k.typ(&mut p, "ripgrep");
    assert_eq!(p.query, "ripgrep");
    k.key_mod(&mut p, KeyCode::Char('x'), KeyModifiers::CONTROL); // ignored
    k.key(&mut p, KeyCode::Backspace);
    k.key(&mut p, KeyCode::Esc); // closes the box, keeps the text, searches nothing
    assert!(!p.input && p.query == "ripgre");
    let s = k.render(&mut p, 150, 30);
    for (w, h) in [(1, 1), (20, 5), (60, 12), (79, 20), (80, 20), (200, 60)] {
        k.render(&mut p, w, h);
    }
    quiet(&p);
    let _ = s;
}

#[test]
fn qa_storage_catalog_install_only_asks() {
    let mut k = Kit::new();
    let mut p = pane(&k, Os::Windows);
    k.key(&mut p, KeyCode::Char('4'));
    k.key(&mut p, KeyCode::Char('j'));
    let picked = p.selected().unwrap();
    k.key(&mut p, KeyCode::Char('i'));
    let q = p.confirm.as_ref().expect("i asks first").question.clone();
    assert_eq!(q, format!("install {}?", CATALOG[picked].name));
    let s = k.render(&mut p, 150, 44);
    assert!(s.contains(&q), "{s}");
    // while asking, other keys (even enter, digits and tab) do nothing
    for code in [KeyCode::Enter, KeyCode::Char('j'), KeyCode::Char('3'), KeyCode::Tab, KeyCode::Char('i'), KeyCode::Char('x')] {
        k.key(&mut p, code);
    }
    assert!(p.confirm.is_some() && p.view == View::Catalog && p.launched.is_empty());
    k.key(&mut p, KeyCode::Char('N'));
    assert!(p.confirm.is_none());
    assert!(k.notices().iter().any(|n| n == "cancelled"));
    // enter asks the same question; esc says no
    k.key(&mut p, KeyCode::Enter);
    assert!(p.confirm.is_some());
    k.key(&mut p, KeyCode::Esc);
    assert!(p.confirm.is_none());
    // double-click asks too (and the category list takes clicks)
    k.render(&mut p, 150, 44);
    let (body, off) = p.table_hit.unwrap();
    let area = Rect::new(0, 0, 150, 44);
    let click = |x, y| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE };
    let _ = off;
    k.mouse(&mut p, click(body.x + 2, body.y), area);
    k.mouse(&mut p, click(body.x + 2, body.y), area);
    assert!(p.confirm.is_some(), "double-click asks");
    k.key(&mut p, KeyCode::Char('n'));
    let (r, c) = p.cat_hits.last().copied().unwrap();
    k.mouse(&mut p, click(r.x + 2, r.y), area);
    assert_eq!(p.cat, c);
    for _ in 0..30 {
        k.mouse(&mut p, MouseEvent { kind: MouseEventKind::ScrollDown, ..click(body.x + 2, body.y) }, area);
    }
    assert!(p.selected().is_some());
    assert!(p.launched.is_empty(), "nothing installed: {:?}", p.launched);
    quiet(&p);
}

#[test]
fn qa_storage_catalog_every_category_every_size() {
    let mut k = Kit::new();
    for os in [Os::Windows, Os::Arch, Os::Debian, Os::Linux] {
        let mut p = pane(&k, os);
        k.key(&mut p, KeyCode::Char('4'));
        for c in 0..=CATS.len() {
            p.set_cat(c);
            for (w, h) in [(1, 1), (3, 2), (10, 4), (30, 8), (79, 20), (80, 20), (81, 24), (120, 30), (150, 44), (260, 80)] {
                k.render(&mut p, w, h);
                k.render_side(&mut p, w.min(34), h);
            }
            k.key(&mut p, KeyCode::End);
            k.render(&mut p, 150, 12);
        }
        // with the box open and a question showing
        k.key(&mut p, KeyCode::Char('/'));
        k.typ(&mut p, "a");
        k.render(&mut p, 150, 44);
        k.key(&mut p, KeyCode::Tab);
        k.key(&mut p, KeyCode::Char('i'));
        for (w, h) in [(1, 1), (20, 6), (60, 15), (150, 44)] {
            k.render(&mut p, w, h);
        }
        k.key(&mut p, KeyCode::Esc);
        quiet(&p);
    }
}
