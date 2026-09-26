//! The lead's typed plan, checked mechanically before any worker starts (docs/ai-research.md, lead mode):
//! every task owns globs, tasks that can run at the same time may not own the same files, "hotspot" files
//! (manifests, lockfiles, module registries, migrations) belong to one scaffold task everything else waits for,
//! sizes are capped, and tiny plans run one task at a time (the solo gate).

use serde_json::Value;

/// One task as the lead planned it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Item {
    /// The lead's own id for it (what depends_on refers to).
    pub key: String,
    pub title: String,
    pub goal: String,
    pub worker: String,
    pub owns: Vec<String>,
    pub reads: Vec<String>,
    pub depends_on: Vec<String>,
    pub acceptance: String,
    /// S | M
    pub size: String,
    pub kind: String,
    /// -1 low … 2 urgent
    pub priority: i8,
}

/// "urgent" / "high" / 2 -> 2; "low" -> -1; anything else normal.
pub fn priority_of(v: &Value) -> i8 {
    match v {
        Value::Number(n) => n.as_i64().unwrap_or(0).clamp(-1, 2) as i8,
        Value::String(s) => match s.trim().to_lowercase().as_str() {
            "urgent" | "critical" | "p0" => 2,
            "high" | "p1" => 1,
            "low" | "p3" => -1,
            _ => 0,
        },
        _ => 0,
    }
}

/// A task already in the run that new ones must not collide with.
pub struct Existing {
    pub key: String,
    pub id: String,
    pub owns: Vec<String>,
    /// still running or waiting (a finished/merged task can't collide any more)
    pub open: bool,
}

pub fn parse_item(v: &Value, i: usize) -> Item {
    let s = |k: &str| v[k].as_str().unwrap_or("").trim().to_string();
    let list = |k: &str| -> Vec<String> {
        match &v[k] {
            Value::Array(a) => a.iter().filter_map(|x| x.as_str()).map(|x| x.trim().replace('\\', "/")).filter(|x| !x.is_empty()).collect(),
            Value::String(s) if !s.trim().is_empty() => s.split(',').map(|x| x.trim().replace('\\', "/")).filter(|x| !x.is_empty()).collect(),
            _ => vec![],
        }
    };
    let mut goal = s("goal");
    if goal.is_empty() {
        goal = s("prompt");
    }
    let mut owns = list("owns");
    if owns.is_empty() {
        owns = list("files");
    }
    let key = if s("id").is_empty() { format!("t{}", i + 1) } else { s("id") };
    Item { key, title: s("title"), goal, worker: s("worker"), owns, reads: list("reads"), depends_on: list("depends_on"), acceptance: s("acceptance"), size: s("size").to_uppercase(), kind: s("kind").to_lowercase(), priority: priority_of(&v["priority"]) }
}

// ------------------------------------------------------------------ globs

/// `*` = anything but '/', `**` = anything, `?` = one char. Case-insensitive (Windows paths).
pub fn glob_match(pat: &str, path: &str) -> bool {
    fn m(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') if p.get(1) == Some(&b'*') => {
                let rest = if p.get(2) == Some(&b'/') { &p[3..] } else { &p[2..] };
                (0..=s.len()).any(|i| m(rest, &s[i..])) || (p.get(2) == Some(&b'/') && m(&p[3..], s))
            }
            Some(b'*') => (0..=s.len()).take_while(|&i| i == 0 || s[i - 1] != b'/').any(|i| m(&p[1..], &s[i..])),
            Some(b'?') => !s.is_empty() && s[0] != b'/' && m(&p[1..], &s[1..]),
            Some(c) => !s.is_empty() && c.eq_ignore_ascii_case(&s[0]) && m(&p[1..], &s[1..]),
        }
    }
    let norm = |x: &str| x.trim().trim_start_matches("./").replace('\\', "/");
    let (p, s) = (norm(pat), norm(path));
    // a bare folder owns everything under it
    let p = if !p.contains(['*', '?']) && (p.ends_with('/') || !p.contains('.')) && !s.eq_ignore_ascii_case(&p) { format!("{}/**", p.trim_end_matches('/')) } else { p };
    m(p.as_bytes(), s.as_bytes())
}

/// The part of a glob before its first wildcard.
fn literal(p: &str) -> String {
    let p = p.trim().trim_start_matches("./").replace('\\', "/");
    p.split(['*', '?']).next().unwrap_or("").to_string()
}

/// Could two globs name the same file?
pub fn overlap(a: &str, b: &str) -> bool {
    let (la, lb) = (literal(a), literal(b));
    if glob_match(a, &lb) || glob_match(b, &la) || glob_match(a, b) || glob_match(b, a) {
        return true;
    }
    // both wildcards: they overlap when one's fixed prefix is inside the other's
    let wild = |p: &str| p.contains(['*', '?']);
    if wild(a) && wild(b) {
        let (x, y) = (la.to_lowercase(), lb.to_lowercase());
        let dir = |s: &str| s.rsplit_once('/').map(|(d, _)| format!("{d}/")).unwrap_or_default();
        let (dx, dy) = (dir(&x), dir(&y));
        return (dx.starts_with(&dy) && b.contains("**")) || (dy.starts_with(&dx) && a.contains("**")) || (dx == dy && (x.starts_with(&y) || y.starts_with(&x)));
    }
    false
}

/// Files whose edits collide no matter how careful the agents are.
pub fn is_hotspot(p: &str) -> bool {
    let p = p.replace('\\', "/").to_lowercase();
    let name = p.rsplit('/').next().unwrap_or(&p);
    matches!(
        name,
        "cargo.toml" | "cargo.lock" | "package.json" | "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock" | "bun.lockb" | "go.mod" | "go.sum" | "pyproject.toml" | "poetry.lock" | "uv.lock" | "gemfile.lock" | "composer.json" | "composer.lock" | "mod.rs" | "lib.rs" | "main.rs" | "__init__.py" | "index.ts" | "index.js" | "index.tsx" | "routes.rb" | "urls.py" | "pnpm-workspace.yaml"
    ) || name.starts_with("requirements") && name.ends_with(".txt")
        || p.contains("migrations/")
        || p.contains("/registry.")
        || name.starts_with("registry.")
        || name.starts_with("routes.")
}

/// Owned entries that name a hotspot ("Cargo.toml", "**/mod.rs", "migrations/**"). A broad glob like "src/**"
/// doesn't count: the plan's overlap check already keeps it away from other tasks.
fn owns_hotspot(owns: &[String]) -> Vec<String> {
    owns.iter().filter(|o| is_hotspot(o)).cloned().collect()
}

// ------------------------------------------------------------------ validation

pub const MAX_FILES: [(&str, usize); 2] = [("S", 3), ("M", 8)];

/// What the checks decided: tasks in dependency order with depends_on resolved, plus notes for the lead.
#[derive(Debug, Default)]
pub struct Checked {
    pub items: Vec<Item>,
    pub notes: Vec<String>,
    /// solo gate: run them one after another
    pub serial: bool,
}

/// Check a plan against itself and the run's open tasks. `workers` = enabled roster names.
pub fn check(items: Vec<Item>, existing: &[Existing], workers: &[String]) -> Result<Checked, Vec<String>> {
    let mut errs = vec![];
    let mut notes = vec![];
    let mut items = items;
    if items.is_empty() {
        return Err(vec!["the plan has no tasks".into()]);
    }
    let known = |k: &str, items: &[Item]| items.iter().any(|i| i.key == k) || existing.iter().any(|e| e.key == k || e.id == k);
    for (n, it) in items.iter().enumerate() {
        let who = if it.title.is_empty() { format!("task {}", n + 1) } else { format!("{:?}", it.title) };
        if it.title.is_empty() || it.goal.is_empty() {
            errs.push(format!("{who}: needs a title and a goal"));
        }
        if !workers.iter().any(|w| w.eq_ignore_ascii_case(&it.worker)) {
            errs.push(format!("{who}: no worker {:?} (roster: {})", it.worker, workers.join(", ")));
        }
        if it.owns.is_empty() {
            errs.push(format!("{who}: say which files it owns (owns: globs like \"src/cli/**\")"));
        }
        match it.size.as_str() {
            "S" | "M" | "" => {}
            "L" | "XL" => errs.push(format!("{who}: size L is too big for one worker — split it into S/M tasks")),
            s => errs.push(format!("{who}: size {s:?} isn't S or M")),
        }
        let cap = MAX_FILES.iter().find(|c| c.0 == if it.size.is_empty() { "M" } else { it.size.as_str() }).map(|c| c.1).unwrap_or(8);
        let literal_files = it.owns.iter().filter(|o| !o.contains(['*', '?'])).count();
        if literal_files > cap {
            errs.push(format!("{who}: owns {literal_files} files, over the {} cap of {cap} — split it", if it.size.is_empty() { "M" } else { &it.size }));
        }
        for d in &it.depends_on {
            if *d == it.key {
                errs.push(format!("{who}: depends on itself"));
            } else if !known(d, &items) {
                errs.push(format!("{who}: depends on {d:?}, which isn't a task id in this plan or run"));
            }
        }
        if items.iter().filter(|x| x.key == it.key).count() > 1 {
            errs.push(format!("task id {:?} is used twice", it.key));
        }
    }
    if !errs.is_empty() {
        errs.dedup();
        return Err(errs);
    }
    // hotspots: one scaffold task owns them, the rest of the plan waits for it
    let hot: Vec<(usize, Vec<String>)> = items.iter().enumerate().map(|(i, it)| (i, owns_hotspot(&it.owns))).filter(|(_, h)| !h.is_empty()).collect();
    if hot.len() > 1 {
        let who: Vec<String> = hot.iter().map(|(i, h)| format!("{:?} ({})", items[*i].title, h.join(", "))).collect();
        return Err(vec![format!("hotspot files (manifests, lockfiles, module registries, migrations) must all belong to ONE scaffold task that merges first; these tasks each own some: {}", who.join("; "))]);
    }
    if let Some((i, h)) = hot.first() {
        let key = items[*i].key.clone();
        let mut added = vec![];
        for (j, it) in items.iter_mut().enumerate() {
            if j != *i && !it.depends_on.contains(&key) {
                it.depends_on.push(key.clone());
                added.push(it.title.clone());
            }
        }
        if !added.is_empty() {
            notes.push(format!("{:?} owns hotspot files ({}), so it merges first: {} wait for it", items[*i].title, h.join(", "), added.join(", ")));
        }
    }
    for e in existing.iter().filter(|e| e.open) {
        if let Some(h) = owns_hotspot(&e.owns).first() {
            if let Some(it) = items.iter().find(|it| owns_hotspot(&it.owns).iter().any(|x| overlap(x, h))) {
                return Err(vec![format!("{:?} owns hotspot {h}, but running task {} already does — make it depend on {} or wait", it.title, e.id, e.key)]);
            }
        }
    }
    // cycles, and which tasks can run at the same time (neither reaches the other)
    let n = items.len();
    let idx = |k: &str, items: &[Item]| items.iter().position(|i| i.key == k);
    let mut reach = vec![vec![false; n]; n];
    for (i, it) in items.iter().enumerate() {
        for d in &it.depends_on {
            if let Some(j) = idx(d, &items) {
                reach[i][j] = true;
            }
        }
    }
    for k in 0..n {
        for i in 0..n {
            for j in 0..n {
                if reach[i][k] && reach[k][j] {
                    reach[i][j] = true;
                }
            }
        }
    }
    if let Some(i) = (0..n).find(|&i| reach[i][i]) {
        return Err(vec![format!("depends_on has a cycle through {:?}", items[i].title)]);
    }
    let mut clash = vec![];
    for i in 0..n {
        for j in i + 1..n {
            if reach[i][j] || reach[j][i] {
                continue;
            }
            if let Some((a, b)) = items[i].owns.iter().flat_map(|a| items[j].owns.iter().map(move |b| (a, b))).find(|(a, b)| overlap(a, b)) {
                clash.push(format!("{:?} and {:?} can run at the same time but both own {a} / {b} — give them disjoint files or add a depends_on", items[i].title, items[j].title));
            }
        }
        for e in existing.iter().filter(|e| e.open) {
            if items[i].depends_on.iter().any(|d| *d == e.key || *d == e.id) {
                continue;
            }
            if let Some((a, b)) = items[i].owns.iter().flat_map(|a| e.owns.iter().map(move |b| (a, b))).find(|(a, b)| overlap(a, b)) {
                clash.push(format!("{:?} owns {a}, which overlaps {b} of task {} that's still open — add depends_on [{:?}] or pick other files", items[i].title, e.id, e.key));
            }
        }
    }
    if !clash.is_empty() {
        return Err(clash);
    }
    // solo gate: a tiny plan runs one task at a time (fan-out costs more than it saves)
    let open = existing.iter().filter(|e| e.open).count();
    let serial = n + open <= 2 && n == 2;
    if serial {
        let first = items[0].key.clone();
        if !items[1].depends_on.contains(&first) && !items[0].depends_on.contains(&items[1].key) {
            items[1].depends_on.push(first);
        }
        notes.push("solo gate: only two tasks, so they run one after the other".into());
    }
    // dependency order
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| (0..n).filter(|&j| reach[i][j]).count());
    let items = order.into_iter().map(|i| items[i].clone()).collect();
    Ok(Checked { items, notes, serial })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn it(key: &str, owns: &[&str], deps: &[&str]) -> Item {
        Item { key: key.into(), title: format!("T {key}"), goal: "do".into(), worker: "codex".into(), owns: owns.iter().map(|s| s.to_string()).collect(), depends_on: deps.iter().map(|s| s.to_string()).collect(), size: "S".into(), ..Default::default() }
    }

    #[test]
    fn agents_plan_globs() {
        assert!(glob_match("src/**", "src/a/b.rs"));
        assert!(glob_match("src/*.rs", "src/a.rs") && !glob_match("src/*.rs", "src/a/b.rs"));
        assert!(glob_match("src/panes", "src/panes/x.rs"), "a bare folder owns what's under it");
        assert!(glob_match("SRC/A.RS", "src/a.rs"));
        assert!(glob_match("**/mod.rs", "src/panes/mod.rs") && glob_match("**/mod.rs", "mod.rs"));
        assert!(overlap("src/cli/**", "src/cli/args.rs"));
        assert!(overlap("src/**", "src/cli/*.rs"));
        assert!(!overlap("src/cli/**", "src/ui/**"));
        assert!(!overlap("docs/a.md", "docs/b.md"));
        assert!(is_hotspot("Cargo.toml") && is_hotspot("src/panes/mod.rs") && is_hotspot("db/migrations/001.sql") && !is_hotspot("src/app.rs"));
    }

    #[test]
    fn agents_plan_checks() {
        let w = vec!["codex".to_string()];
        // disjoint files: fine, and three tasks fan out
        let ok = check(vec![it("a", &["src/a.rs"], &[]), it("b", &["src/b.rs"], &[]), it("c", &["docs/**"], &[])], &[], &w).unwrap();
        assert!(!ok.serial && ok.items.len() == 3);
        // overlapping owns for concurrent tasks: refused; a dependency makes it fine
        let e = check(vec![it("a", &["src/**"], &[]), it("b", &["src/b.rs"], &[]), it("c", &["x.md"], &[])], &[], &w).unwrap_err();
        assert!(e[0].contains("both own"), "{e:?}");
        assert!(check(vec![it("a", &["src/**"], &[]), it("b", &["src/b.rs"], &["a"]), it("c", &["x.md"], &[])], &[], &w).is_ok());
        // hotspots: two owners refused; one owner becomes the scaffold everyone waits for
        let e = check(vec![it("a", &["Cargo.toml", "src/a.rs"], &[]), it("b", &["package.json"], &[]), it("c", &["x.md"], &[])], &[], &w).unwrap_err();
        assert!(e[0].contains("hotspot"), "{e:?}");
        let s = check(vec![it("b", &["src/b.rs"], &[]), it("a", &["Cargo.toml"], &[]), it("c", &["x.md"], &[])], &[], &w).unwrap();
        assert_eq!(s.items[0].key, "a", "the scaffold comes first");
        assert!(s.items[1..].iter().all(|i| i.depends_on.contains(&"a".to_string())));
        // sizes, workers, deps, cycles
        let mut big = it("a", &["a", "b", "c", "d"], &[]);
        big.size = "S".into();
        assert!(check(vec![big], &[], &w).unwrap_err()[0].contains("cap"));
        let mut l = it("a", &["x"], &[]);
        l.size = "L".into();
        assert!(check(vec![l], &[], &w).unwrap_err()[0].contains("split"));
        let mut nw = it("a", &["x"], &[]);
        nw.worker = "gpt".into();
        assert!(check(vec![nw], &[], &w).unwrap_err()[0].contains("no worker"));
        assert!(check(vec![it("a", &["x"], &["zz"])], &[], &w).unwrap_err()[0].contains("isn't a task id"));
        assert!(check(vec![it("a", &["x"], &["b"]), it("b", &["y"], &["a"]), it("c", &["z"], &[])], &[], &w).unwrap_err()[0].contains("cycle"));
        // solo gate: two tasks run one after the other
        let solo = check(vec![it("a", &["x.rs"], &[]), it("b", &["y.rs"], &[])], &[], &w).unwrap();
        assert!(solo.serial && solo.items[1].depends_on == vec!["a".to_string()]);
        // an open task elsewhere in the run counts
        let ex = [Existing { key: "old".into(), id: "k1".into(), owns: vec!["src/**".into()], open: true }];
        assert!(check(vec![it("a", &["src/x.rs"], &[])], &ex, &w).unwrap_err()[0].contains("still open"));
        assert!(check(vec![it("a", &["src/x.rs"], &["old"])], &ex, &w).is_ok());
    }
}
