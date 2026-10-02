//! Checkpoints: before each reply a coding agent (Claude Code, Codex) starts, the chat's folder is snapshotted
//! into git without touching anything you use: HEAD, branches, the stash and the real index stay as they are.
//! A folder in a repo gets a commit under `refs/oriel/ckpt/<chat>/<turn>`, made from a scratch index (committed,
//! modified and untracked files, .gitignore respected); any other folder gets a shadow git-dir in oriel's data
//! folder. /diff, /undo and /rewind read them back. All of it blocks: call it from background threads only.

use crate::panes::agents::git;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

/// Left out of every snapshot on top of .gitignore: dependencies and build output that can be made again.
pub const EXCLUDES: &[&str] = &["node_modules/", "target/", "dist/", "build/", ".venv/", "__pycache__/", "*.log"];

/// A folder too big to snapshot quickly is skipped (with the reason) rather than copied into git.
pub struct Caps {
    /// new (untracked) files
    pub files: usize,
    pub bytes: u64,
    /// how long just listing them may take
    pub secs: u64,
}

pub const CAPS: Caps = Caps { files: 20_000, bytes: 300 << 20, secs: 8 };

/// Checkpoints kept per chat, and for how long.
const KEEP: usize = 50;
const KEEP_DAYS: i64 = 14;

/// Who checkpoint commits are by (a repo without user.name set can still make them).
const IDENT: [(&str, &str); 4] = [("GIT_AUTHOR_NAME", "oriel"), ("GIT_AUTHOR_EMAIL", "oriel@localhost"), ("GIT_COMMITTER_NAME", "oriel"), ("GIT_COMMITTER_EMAIL", "oriel@localhost")];

/// One git job at a time per folder: two at once (a turn's snapshot, /diff, the note's numbers) would fight over
/// the shadow's index (index.lock) or snapshot a folder half way through a restore.
static BUSY: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
static FREED: std::sync::Condvar = std::sync::Condvar::new();

/// Holds a folder until dropped.
struct Busy(PathBuf);

fn busy(root: &Path) -> Busy {
    let mut b = BUSY.lock().unwrap_or_else(|e| e.into_inner());
    while b.iter().any(|r| r == root) {
        b = FREED.wait(b).unwrap_or_else(|e| e.into_inner());
    }
    b.push(root.to_path_buf());
    Busy(root.to_path_buf())
}

impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.lock().unwrap_or_else(|e| e.into_inner()).retain(|r| *r != self.0);
        FREED.notify_all();
    }
}

/// Where a folder's checkpoints live.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    /// What gets snapshotted: the repo's top folder, or the chat's folder itself.
    pub root: PathBuf,
    /// A folder that isn't in a repo (or that its repo ignores): the shadow git-dir kept for it.
    pub shadow: Option<PathBuf>,
}

/// +/- totals between two checkpoints.
pub use git::Stats;

/// What restoring a checkpoint would do: files it writes back, and files made since that it deletes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub write: Vec<String>,
    pub delete: Vec<String>,
}

impl Place {
    pub fn of(dir: &Path) -> Result<Place, String> {
        if crate::config::which("git").is_none() {
            return Err("git isn't installed".into());
        }
        let dir = std::path::absolute(dir).map_err(|e| e.to_string())?;
        if !dir.is_dir() {
            return Err(format!("{} isn't a folder", dir.display()));
        }
        // in a repo that doesn't ignore it: the repo's own objects and refs
        if let Ok(top) = git::ok(&dir, &["rev-parse", "--show-toplevel"]) {
            if !git::run(&dir, &["check-ignore", "-q", "."]).ok {
                return Ok(Place { root: PathBuf::from(top.replace('/', std::path::MAIN_SEPARATOR_STR)), shadow: None });
            }
        }
        let shadow = shadow_root().join(format!("{:016x}", fnv(&dir.to_string_lossy())));
        let p = Place { root: dir, shadow: Some(shadow.clone()) };
        let _busy = busy(&p.root);
        if !shadow.join("HEAD").is_file() {
            std::fs::create_dir_all(&shadow).map_err(|e| format!("couldn't make {}: {e}", shadow.display()))?;
            p.ok(&["init", "-q"])?;
            // byte for byte: a restored file is exactly what was there
            p.ok(&["config", "core.autocrlf", "false"])?;
            let _ = std::fs::create_dir_all(shadow.join("info"));
            let _ = std::fs::write(shadow.join("info").join("exclude"), EXCLUDES.join("\n") + "\n");
            let _ = std::fs::write(shadow.join("oriel-folder"), p.root.to_string_lossy().as_bytes());
        }
        Ok(p)
    }

    fn git_env(&self, args: &[&str], extra: &[(&str, &str)]) -> git::Out {
        let (d, w) = (self.shadow.as_ref().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(), self.root.to_string_lossy().to_string());
        let mut env: Vec<(&str, &str)> = extra.to_vec();
        if self.shadow.is_some() {
            env.extend([("GIT_DIR", d.as_str()), ("GIT_WORK_TREE", w.as_str())]);
        }
        git::run_env(&self.root, args, &env)
    }

    fn ok(&self, args: &[&str]) -> Result<String, String> {
        ok_of(self.git_env(args, &[]), args)
    }
}

fn ok_of(o: git::Out, args: &[&str]) -> Result<String, String> {
    if o.ok {
        return Ok(o.stdout.trim().to_string());
    }
    let msg = o.stderr.lines().chain(o.stdout.lines()).map(str::trim).find(|l| !l.is_empty() && !l.starts_with("hint:") && !l.starts_with("warning:")).unwrap_or("").to_string();
    Err(if msg.is_empty() { format!("git {} failed ({})", args.first().unwrap_or(&""), o.code) } else { msg.trim_start_matches("fatal: ").trim_start_matches("error: ").to_string() })
}

/// Shadow git-dirs, one per folder that isn't a repo (tests keep theirs to themselves).
fn shadow_root() -> PathBuf {
    #[cfg(test)]
    {
        std::path::absolute("target/test-scratch/chat/ckpt-shadows").unwrap_or_default()
    }
    #[cfg(not(test))]
    {
        crate::config::data_dir().join("ckpt")
    }
}

/// FNV-1a: a folder's shadow keeps its name from one oriel version to the next.
fn fnv(s: &str) -> u64 {
    let s = if cfg!(windows) { s.to_lowercase().replace('/', "\\") } else { s.to_string() };
    s.trim_end_matches(['/', '\\']).bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

/// The extra excludes as pathspecs for a repo's `add -A`.
fn pathspecs() -> Vec<String> {
    EXCLUDES
        .iter()
        .map(|e| match e.strip_suffix('/') {
            Some(dir) => format!(":(exclude,glob)**/{dir}/**"),
            None => format!(":(exclude,glob)**/{e}"),
        })
        .collect()
}

/// Folders found too big lately, with why: they aren't listed again for a while (in a huge folder the listing is
/// the slow part, and every turn would wait for it).
static TOO_BIG: std::sync::Mutex<Vec<(PathBuf, usize, u64, Instant, String)>> = std::sync::Mutex::new(Vec::new());
const TOO_BIG_FOR: Duration = Duration::from_secs(600);

/// Too many or too big new files to snapshot quickly: why, else Ok.
fn size_check(p: &Place, caps: &Caps) -> Result<(), String> {
    let seen = |e: &(PathBuf, usize, u64, Instant, String)| e.0 == p.root && e.1 == caps.files && e.2 == caps.bytes;
    if let Some(e) = TOO_BIG.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|e| seen(e) && e.3.elapsed() < TOO_BIG_FOR) {
        return Err(e.4.clone());
    }
    let r = list_check(p, caps);
    let mut memo = TOO_BIG.lock().unwrap_or_else(|e| e.into_inner());
    memo.retain(|e| !seen(e) && e.3.elapsed() < TOO_BIG_FOR);
    if let Err(why) = &r {
        memo.push((p.root.clone(), caps.files, caps.bytes, Instant::now(), why.clone()));
    }
    r
}

fn list_check(p: &Place, caps: &Caps) -> Result<(), String> {
    let t0 = Instant::now();
    let mut args = vec!["ls-files", "--others", "--exclude-standard", "-z"];
    if p.shadow.is_none() {
        for e in EXCLUDES {
            args.extend(["-x", e]);
        }
    }
    let out = ok_of(p.git_env(&args, &[]), &args)?;
    if t0.elapsed() > Duration::from_secs(caps.secs) {
        return Err(format!("listing the folder took {}s", t0.elapsed().as_secs()));
    }
    let files: Vec<&str> = out.split('\0').filter(|f| !f.is_empty()).collect();
    if files.len() > caps.files {
        return Err(format!("{} new files (over {}) — put the ones you don't need in .gitignore", files.len(), caps.files));
    }
    let bytes: u64 = files.iter().map(|f| std::fs::metadata(p.root.join(f)).map(|m| m.len()).unwrap_or(0)).sum();
    if bytes > caps.bytes {
        return Err(format!("{} MB of new files (over {} MB) — put the big ones in .gitignore", bytes >> 20, caps.bytes >> 20));
    }
    Ok(())
}

/// The folder as it is now, as a git tree. Nothing you use changes; a folder too big to do quickly is skipped
/// with the reason.
pub fn tree(p: &Place, caps: &Caps) -> Result<String, String> {
    let _busy = busy(&p.root);
    tree_in(p, caps)
}

fn tree_in(p: &Place, caps: &Caps) -> Result<String, String> {
    size_check(p, caps)?;
    match &p.shadow {
        None => {
            let spec = pathspecs();
            let spec: Vec<&str> = spec.iter().map(String::as_str).collect();
            let o = git::snapshot_index(&p.root, &spec, |env| git::run_env(&p.root, &["write-tree"], env))?;
            ok_of(o, &["write-tree"])
        }
        Some(_) => {
            // the shadow's own index is the stat cache: only what changed gets hashed again
            p.ok(&["add", "-A"])?;
            p.ok(&["write-tree"])
        }
    }
}

/// Snapshot the folder as checkpoint `turn` of `chat`: the commit's sha. Old checkpoints are pruned on the way.
pub fn snapshot(p: &Place, chat: &str, turn: usize, caps: &Caps) -> Result<String, String> {
    let _busy = busy(&p.root);
    let tree = tree_in(p, caps)?;
    let msg = format!("oriel checkpoint · chat {chat} · turn {turn}");
    let args = ["commit-tree", tree.as_str(), "-m", msg.as_str()];
    let c = ok_of(p.git_env(&args, &IDENT), &args)?;
    p.ok(&["update-ref", &format!("refs/oriel/ckpt/{chat}/{turn}"), &c])?;
    prune_in(p, chat, crate::panes::files::clock::now_secs());
    Ok(c)
}

/// Keep a chat's newest KEEP checkpoints, and nobody's older than KEEP_DAYS (a deleted chat's go that way too).
#[cfg(test)]
pub fn prune(p: &Place, chat: &str, now: i64) {
    let _busy = busy(&p.root);
    prune_in(p, chat, now)
}

fn prune_in(p: &Place, chat: &str, now: i64) {
    let Ok(list) = p.ok(&["for-each-ref", "--format=%(refname) %(creatordate:unix)", "refs/oriel/ckpt/"]) else { return };
    let mut mine: Vec<(usize, String)> = vec![];
    let mut drop: Vec<String> = vec![];
    for l in list.lines() {
        let Some((r, ts)) = l.rsplit_once(' ') else { continue };
        if ts.parse::<i64>().is_ok_and(|t| now - t > KEEP_DAYS * 86400) {
            drop.push(r.to_string());
        } else if let Some(n) = r.strip_prefix(&format!("refs/oriel/ckpt/{chat}/")).and_then(|n| n.parse().ok()) {
            mine.push((n, r.to_string()));
        }
    }
    mine.sort_by(|a, b| b.0.cmp(&a.0));
    drop.extend(mine.into_iter().skip(KEEP).map(|m| m.1));
    for r in &drop {
        let _ = p.ok(&["update-ref", "-d", r]);
    }
    if !drop.is_empty() && p.shadow.is_some() {
        let _ = p.ok(&["gc", "--auto", "--quiet"]);
    }
}

/// +/- totals from one checkpoint (or tree) to another.
pub fn stat(p: &Place, from: &str, to: &str) -> Result<Stats, String> {
    let out = p.ok(&["diff", "--numstat", "--no-renames", from, to])?;
    let mut s = Stats::default();
    for l in out.lines() {
        let mut it = l.split('\t');
        s.added += it.next().and_then(|a| a.parse::<u64>().ok()).unwrap_or(0);
        s.removed += it.next().and_then(|r| r.parse::<u64>().ok()).unwrap_or(0);
        s.files += 1;
    }
    Ok(s)
}

/// The changes from `from` to `to`, file by file, for the diff view.
pub fn diff(p: &Place, from: &str, to: &str) -> Result<Vec<git::DiffFile>, String> {
    let out = p.ok(&["-c", "core.quotepath=false", "diff", "--no-color", "--no-ext-diff", "-M", from, to])?;
    Ok(git::parse_diff(&out))
}

/// Is `ckpt` one of this folder's?
pub fn exists(p: &Place, ckpt: &str) -> bool {
    p.git_env(&["cat-file", "-e", &format!("{ckpt}^{{tree}}")], &[]).ok
}

/// What going back to `ckpt` from `now` (a tree of the folder as it is) would write and delete.
pub fn plan(p: &Place, ckpt: &str, now: &str) -> Result<Plan, String> {
    if !exists(p, ckpt) {
        return Err("that checkpoint isn't in this folder's history (was the chat's folder changed?)".into());
    }
    let out = p.ok(&["-c", "core.quotepath=false", "diff-tree", "-r", "--no-renames", "--name-status", "-z", now, ckpt])?;
    let mut plan = Plan::default();
    let mut it = out.split('\0').filter(|s| !s.is_empty());
    while let (Some(st), Some(path)) = (it.next(), it.next()) {
        match st.chars().next() {
            Some('D') => plan.delete.push(path.to_string()),
            Some(_) => plan.write.push(path.to_string()),
            None => {}
        }
    }
    Ok(plan)
}

/// Put the folder back the way it was at `ckpt`: the files in `plan.write` come back from it, the ones in
/// `plan.delete` (made since) go. Only those paths are touched; the real index, HEAD and branches never are.
pub fn restore(p: &Place, ckpt: &str, plan: &Plan) -> Result<(), String> {
    let _busy = busy(&p.root);
    let safe = |f: &String| !Path::new(f).is_absolute() && Path::new(f).components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if let Some(bad) = plan.write.iter().chain(&plan.delete).find(|f| !safe(f)) {
        return Err(format!("refusing a path outside the folder: {bad}"));
    }
    if !plan.write.is_empty() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let tmp = std::env::temp_dir().join(format!("oriel-restore-{}-{nanos}", std::process::id()));
        let t = tmp.to_string_lossy().to_string();
        let env = [("GIT_INDEX_FILE", t.as_str())];
        let r = (|| {
            ok_of(p.git_env(&["read-tree", ckpt], &env), &["read-tree"])?;
            // in batches that keep the command line short (Windows caps it)
            let mut batch: Vec<&str> = vec![];
            let mut len = 0;
            for (i, f) in plan.write.iter().enumerate() {
                batch.push(f);
                len += f.len() + 3;
                if len > 6000 || i + 1 == plan.write.len() {
                    let mut args = vec!["checkout-index", "-f", "--"];
                    args.append(&mut batch);
                    ok_of(p.git_env(&args, &env), &["checkout-index"])?;
                    len = 0;
                }
            }
            Ok::<(), String>(())
        })();
        let _ = std::fs::remove_file(&tmp);
        r?;
    }
    for f in &plan.delete {
        let path = p.root.join(f);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("couldn't delete {f}: {e}")),
        }
        // folders it leaves empty go too (never the folder itself)
        let mut d = path.parent();
        while let Some(dir) = d.filter(|d| d.starts_with(&p.root) && *d != p.root) {
            if std::fs::remove_dir(dir).is_err() {
                break;
            }
            d = dir.parent();
        }
    }
    Ok(())
}

/// A chat's checkpoints in this folder, newest turn first: (turn, commit).
#[cfg(test)]
pub fn list(p: &Place, chat: &str) -> Vec<(usize, String)> {
    let out = p.ok(&["for-each-ref", "--format=%(refname) %(objectname)", &format!("refs/oriel/ckpt/{chat}/")]).unwrap_or_default();
    let mut v: Vec<(usize, String)> = out.lines().filter_map(|l| l.split_once(' ')).filter_map(|(r, sha)| Some((r.rsplit('/').next()?.parse().ok()?, sha.to_string()))).collect();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::path::absolute(format!("target/test-scratch/chat/ckpt/{name}")).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(d: &Path, f: &str, text: &str) {
        let p = d.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn read(d: &Path, f: &str) -> Option<String> {
        std::fs::read_to_string(d.join(f)).ok()
    }

    /// A folder that isn't a repo: a shadow git-dir snapshots it (dependencies and logs left out), and going back
    /// writes the old files byte for byte and deletes the ones made since, empty folders too.
    #[test]
    fn chat_ckpt_shadow_round_trip() {
        let d = scratch("plain");
        write(&d, "a.txt", "one\r\ntwo\n");
        write(&d, "src/b.rs", "fn b() {}\n");
        write(&d, "node_modules/x/huge.js", "dependency");
        write(&d, "run.log", "noise");
        let p = Place::of(&d).unwrap();
        assert!(p.shadow.is_some(), "target/ is ignored by oriel's repo: a shadow, not oriel's own refs");
        let c1 = snapshot(&p, "chat1", 1, &CAPS).unwrap();
        // the agent's turn: an edit, a new file in a new folder, a deletion, and noise in excluded places
        write(&d, "a.txt", "ONE\n");
        write(&d, "src/new/c.rs", "fn c() {}\n");
        std::fs::remove_file(d.join("src/b.rs")).unwrap();
        write(&d, "node_modules/x/huge.js", "changed dependency");
        let now = tree(&p, &CAPS).unwrap();
        let s = stat(&p, &c1, &now).unwrap();
        assert_eq!((s.files, s.added, s.removed), (3, 2, 3), "{s:?}");
        let files = diff(&p, &c1, &now).unwrap();
        assert!(files.iter().any(|f| f.path == "src/new/c.rs" && f.note == "new"), "{:?}", files.iter().map(|f| &f.path).collect::<Vec<_>>());
        let plan = plan(&p, &c1, &now).unwrap();
        assert_eq!(plan.delete, ["src/new/c.rs"]);
        assert_eq!(plan.write.len(), 2, "{plan:?}");
        restore(&p, &c1, &plan).unwrap();
        assert_eq!(read(&d, "a.txt").as_deref(), Some("one\r\ntwo\n"), "exact bytes, line endings too");
        assert_eq!(read(&d, "src/b.rs").as_deref(), Some("fn b() {}\n"));
        assert!(!d.join("src/new").exists(), "the new file and the folder it made are gone");
        assert_eq!(read(&d, "node_modules/x/huge.js").as_deref(), Some("changed dependency"), "excluded folders are never touched");
        assert_eq!(read(&d, "run.log").as_deref(), Some("noise"));
        // back where it was: nothing left to do
        let again = super::plan(&p, &c1, &tree(&p, &CAPS).unwrap()).unwrap();
        assert!(again.write.is_empty() && again.delete.is_empty(), "{again:?}");
        assert_eq!(list(&p, "chat1"), [(1, c1)]);
    }

    /// In a repo: checkpoints are commits under refs/oriel/ckpt/, taken from a scratch index, so HEAD, the branch,
    /// the real index and the stash are exactly as they were, before and after a restore.
    #[test]
    fn chat_ckpt_repo_leaves_git_alone() {
        let d = scratch("repo");
        let g = |args: &[&str]| git::ok(&d, args).unwrap();
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);
        g(&["config", "core.autocrlf", "false"]);
        write(&d, "lib.rs", "pub fn a() {}\n");
        write(&d, ".gitignore", "secret/\n");
        g(&["add", "-A"]);
        g(&["commit", "-q", "-m", "first"]);
        write(&d, "lib.rs", "pub fn a() { 1 }\n"); // uncommitted
        write(&d, "notes.md", "untracked\n");
        write(&d, "secret/key", "ignored");
        g(&["add", "notes.md"]); // staged
        let before = (g(&["rev-parse", "HEAD"]), g(&["status", "--porcelain"]), g(&["stash", "list"]));
        let p = Place::of(&d).unwrap();
        assert!(p.shadow.is_none() && p.root == d, "{p:?}");
        let c = snapshot(&p, "c2", 3, &CAPS).unwrap();
        assert_eq!((g(&["rev-parse", "HEAD"]), g(&["status", "--porcelain"]), g(&["stash", "list"])), before, "snapshotting changed nothing");
        assert!(g(&["show-ref"]).contains("refs/oriel/ckpt/c2/3"));
        write(&d, "lib.rs", "broken\n");
        std::fs::remove_file(d.join("notes.md")).unwrap();
        write(&d, "extra.rs", "x\n");
        let now = tree(&p, &CAPS).unwrap();
        let plan = plan(&p, &c, &now).unwrap();
        assert_eq!((plan.write.clone(), plan.delete.clone()), (vec!["lib.rs".to_string(), "notes.md".to_string()], vec!["extra.rs".to_string()]));
        restore(&p, &c, &plan).unwrap();
        assert_eq!(read(&d, "lib.rs").as_deref(), Some("pub fn a() { 1 }\n"));
        assert_eq!(read(&d, "notes.md").as_deref(), Some("untracked\n"));
        assert!(!d.join("extra.rs").exists() && d.join("secret/key").exists(), "ignored files are never snapshotted or deleted");
        assert_eq!((g(&["rev-parse", "HEAD"]), g(&["status", "--porcelain"]), g(&["stash", "list"])), before, "back to exactly the state at the checkpoint");
    }

    /// Snapshots, trees and restores of one folder never run at once (they'd fight over the shadow's index.lock):
    /// several threads going at the same folder all succeed.
    #[test]
    fn chat_ckpt_one_job_per_folder() {
        let d = scratch("busy");
        for i in 0..20 {
            write(&d, &format!("f{i}.txt"), &"x".repeat(i * 100));
        }
        let p = Place::of(&d).unwrap();
        let jobs: Vec<_> = (0..6)
            .map(|n| {
                let p = p.clone();
                std::thread::spawn(move || if n % 2 == 0 { snapshot(&p, "busy", n, &CAPS).map(|_| ()) } else { tree(&p, &CAPS).map(|_| ()) })
            })
            .collect();
        for j in jobs {
            j.join().unwrap().unwrap();
        }
        assert_eq!(list(&p, "busy").len(), 3);
    }

    /// A chat keeps its newest 50 checkpoints; anything older than two weeks goes whoever it belongs to; a folder
    /// with too many new files is skipped with the reason.
    #[test]
    fn chat_ckpt_prunes_and_caps() {
        let d = scratch("prune");
        write(&d, "a.txt", "a");
        let p = Place::of(&d).unwrap();
        let t = tree(&p, &CAPS).unwrap();
        let c = ok_of(p.git_env(&["commit-tree", &t, "-m", "x"], &IDENT), &[]).unwrap();
        for n in 0..53 {
            p.ok(&["update-ref", &format!("refs/oriel/ckpt/busy/{n}"), &c]).unwrap();
        }
        p.ok(&["update-ref", "refs/oriel/ckpt/other/1", &c]).unwrap();
        prune(&p, "busy", crate::panes::files::clock::now_secs());
        let left = list(&p, "busy");
        assert_eq!((left.len(), left[0].0, left[49].0), (50, 52, 3), "the newest 50");
        assert_eq!(list(&p, "other").len(), 1, "another chat's recent ones stay");
        prune(&p, "busy", crate::panes::files::clock::now_secs() + 15 * 86400);
        assert!(list(&p, "busy").is_empty() && list(&p, "other").is_empty(), "two weeks on, all gone");
        for i in 0..5 {
            write(&d, &format!("new{i}.txt"), "x");
        }
        let err = tree(&p, &Caps { files: 3, bytes: 1 << 30, secs: 60 }).unwrap_err();
        assert!(err.contains("new files (over 3)"), "{err}");
        // said again at once for a while, without listing the folder again
        for i in 5..10 {
            write(&d, &format!("new{i}.txt"), "x");
        }
        assert_eq!(tree(&p, &Caps { files: 3, bytes: 1 << 30, secs: 60 }).unwrap_err(), err, "remembered, not listed again");
        assert!(tree(&p, &Caps { files: 100, bytes: 2, secs: 60 }).unwrap_err().contains("MB of new files"));
        assert_eq!(fnv("C:\\Work\\X\\"), fnv(if cfg!(windows) { "c:/work/x" } else { "C:\\Work\\X" }));
    }
}
