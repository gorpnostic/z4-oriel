//! Every git operation the orchestrator does. All of it is blocking, so the pane only ever calls these from
//! background threads. Commands spawn without a console window on Windows and never prompt.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Out {
    pub ok: bool,
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

fn command(prog: &str) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(prog);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c.stdin(std::process::Stdio::null());
    c
}

/// Run git in `dir` with extra environment variables.
pub fn run_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Out {
    let mut c = command("git");
    c.arg("-C").arg(dir).args(args);
    c.env("GIT_TERMINAL_PROMPT", "0").env("GIT_EDITOR", "true").env("GIT_MERGE_AUTOEDIT", "no").env("LC_ALL", "C");
    for (k, v) in env {
        c.env(k, v);
    }
    match c.output() {
        Ok(o) => Out {
            ok: o.status.success(),
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).to_string(),
            stderr: String::from_utf8_lossy(&o.stderr).to_string(),
        },
        Err(e) => Out { ok: false, code: -1, stdout: String::new(), stderr: format!("couldn't run git: {e}") },
    }
}

pub fn run(dir: &Path, args: &[&str]) -> Out {
    run_env(dir, args, &[])
}

/// stdout trimmed, or the first line of stderr as the error.
pub fn ok(dir: &Path, args: &[&str]) -> Result<String, String> {
    let o = run(dir, args);
    if o.ok { Ok(o.stdout.trim().to_string()) } else { Err(err_line(&o, args)) }
}

fn err_line(o: &Out, args: &[&str]) -> String {
    let msg = o.stderr.lines().chain(o.stdout.lines()).map(str::trim).find(|l| !l.is_empty() && !l.starts_with("hint:")).unwrap_or("").to_string();
    let msg = msg.trim_start_matches("fatal: ").trim_start_matches("error: ").to_string();
    if msg.is_empty() { format!("git {} failed ({})", args.first().unwrap_or(&""), o.code) } else { msg }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RepoInfo {
    pub root: PathBuf,
    pub name: String,
    pub branch: String,
}

/// The repo containing `dir` (its top-level folder, name and current branch).
pub fn repo_info(dir: &Path) -> Result<RepoInfo, String> {
    if !dir.is_dir() {
        return Err(format!("{} isn't a folder", dir.display()));
    }
    let root = ok(dir, &["rev-parse", "--show-toplevel"]).map_err(|_| format!("{} isn't in a git repo", dir.display()))?;
    let root = PathBuf::from(root.replace('/', std::path::MAIN_SEPARATOR_STR));
    let branch = ok(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|_| "(no commits)".into());
    let name = root.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| root.display().to_string());
    Ok(RepoInfo { root, name, branch })
}

pub struct Started {
    pub branch: String,
    pub base_branch: String,
    pub base_sha: String,
    pub worktree: PathBuf,
}

/// `git worktree add -b oriel/<slug> <wt> HEAD`, then the per-worktree hooks file (kept out of commits via
/// info/exclude). `hook_exe` = the oriel binary the hooks call; None skips the hooks (Codex/Kimi).
#[cfg(test)]
pub fn start(repo: &Path, wt: &Path, slug: &str, task_id: &str, hook_exe: Option<&Path>) -> Result<Started, String> {
    start_at(repo, wt, slug, task_id, hook_exe, None)
}

/// Like `start`, but branched off `base` (a lead run's integration branch) instead of the checked-out HEAD.
pub fn start_at(repo: &Path, wt: &Path, slug: &str, task_id: &str, hook_exe: Option<&Path>, base: Option<&str>) -> Result<Started, String> {
    let (base_sha, base_branch) = match base {
        Some(b) => (ok(repo, &["rev-parse", &format!("refs/heads/{b}")]).map_err(|_| format!("the branch {b} is gone"))?, b.to_string()),
        None => (
            ok(repo, &["rev-parse", "HEAD"]).map_err(|_| "the repo has no commits yet — commit something first".to_string())?,
            ok(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|_| "HEAD".into()),
        ),
    };
    let branch = format!("oriel/{slug}");
    if let Some(p) = wt.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("couldn't make {}: {e}", p.display()))?;
    }
    // a leftover from an earlier run of the same task (retry): clear it first
    if wt.exists() {
        let _ = run(repo, &["worktree", "remove", "--force", &wt.to_string_lossy()]);
        let _ = std::fs::remove_dir_all(wt);
        let _ = run(repo, &["worktree", "prune"]);
    }
    if run(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).ok {
        let _ = run(repo, &["branch", "-D", &branch]);
    }
    ok(repo, &["worktree", "add", "-b", &branch, &wt.to_string_lossy(), &base_sha])?;
    if let Some(exe) = hook_exe {
        write_hooks(wt, task_id, exe).map_err(|e| format!("worktree made, but couldn't write its hooks: {e}"))?;
        exclude(wt, ".claude/settings.local.json");
    }
    Ok(Started { branch, base_branch, base_sha, worktree: wt.to_path_buf() })
}

/// Claude Code hooks that report this task's state back to oriel (docs/ai-research.md §3).
pub fn hooks_json(task_id: &str, exe: &Path) -> serde_json::Value {
    // forward slashes work in both cmd and Git Bash (which Claude uses for hooks on Windows)
    let exe = exe.to_string_lossy().replace('\\', "/");
    let cmd = |state: &str| serde_json::json!([{ "hooks": [{ "type": "command", "command": format!("\"{exe}\" report --task {task_id} --state {state}"), "timeout": 10 }] }]);
    let tool = serde_json::json!([{ "matcher": "*", "hooks": [{ "type": "command", "command": format!("\"{exe}\" report --task {task_id} --state running"), "timeout": 10 }] }]);
    serde_json::json!({
        "hooks": {
            "SessionStart": cmd("session"),
            "UserPromptSubmit": cmd("running"),
            "PreToolUse": tool,
            "Notification": cmd("blocked"),
            "Stop": cmd("idle"),
        }
    })
}

/// Hooks for a worktree made without them (a headless task the user takes over in a terminal tab).
pub fn add_hooks(wt: &Path, task_id: &str, exe: &Path) -> Result<(), String> {
    write_hooks(wt, task_id, exe).map_err(|e| format!("couldn't write the hooks: {e}"))?;
    exclude(wt, ".claude/settings.local.json");
    Ok(())
}

fn write_hooks(wt: &Path, task_id: &str, exe: &Path) -> std::io::Result<()> {
    let dir = wt.join(".claude");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("settings.local.json");
    // keep anything already there (a repo could commit one); ours only adds the hooks key
    let mut v: serde_json::Value = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| serde_json::json!({}));
    if !v.is_object() {
        v = serde_json::json!({});
    }
    v["hooks"] = hooks_json(task_id, exe)["hooks"].clone();
    super::store::write_atomic(&path, serde_json::to_string_pretty(&v).unwrap_or_default().as_bytes())
}

/// Keep a path out of commits (the repo's info/exclude).
pub fn exclude_path(wt: &Path, pattern: &str) {
    exclude(wt, pattern)
}

/// Add a pattern to the repo's info/exclude (shared by all its worktrees) if it isn't there yet.
fn exclude(wt: &Path, pattern: &str) {
    let Ok(common) = ok(wt, &["rev-parse", "--git-common-dir"]) else { return };
    let common = if Path::new(&common).is_absolute() { PathBuf::from(&common) } else { wt.join(&common) };
    let path = common.join("info").join("exclude");
    let cur = std::fs::read_to_string(&path).unwrap_or_default();
    if cur.lines().any(|l| l.trim() == pattern) {
        return;
    }
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let mut s = cur;
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(pattern);
    s.push('\n');
    let _ = std::fs::write(&path, s);
}

/// Run `f` with a scratch index holding the worktree's full state (committed + uncommitted + untracked), so
/// diffs see new files without touching the agent's real index.
fn with_snapshot_index<R>(wt: &Path, f: impl FnOnce(&[(&str, &str)]) -> R) -> Result<R, String> {
    let real = ok(wt, &["rev-parse", "--git-path", "index"])?;
    let real = if Path::new(&real).is_absolute() { PathBuf::from(&real) } else { wt.join(&real) };
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("oriel-idx-{}-{nanos}", std::process::id()));
    let _ = std::fs::copy(&real, &tmp); // keeps git's stat cache so `add -A` doesn't rehash everything
    let t = tmp.to_string_lossy().to_string();
    let env = [("GIT_INDEX_FILE", t.as_str())];
    let o = run_env(wt, &["add", "-A"], &env);
    let r = if o.ok { Ok(f(&env)) } else { Err(err_line(&o, &["add"])) };
    let _ = std::fs::remove_file(&tmp);
    r
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub added: u64,
    pub removed: u64,
    pub files: u64,
}

/// +/- totals of the worktree against the task's base commit.
pub fn stats(wt: &Path, base_sha: &str) -> Result<Stats, String> {
    with_snapshot_index(wt, |env| {
        let o = run_env(wt, &["diff", "--cached", "--numstat", base_sha], env);
        let mut s = Stats::default();
        for l in o.stdout.lines() {
            let mut it = l.split('\t');
            let (a, r) = (it.next().unwrap_or("0"), it.next().unwrap_or("0"));
            s.added += a.parse::<u64>().unwrap_or(0);
            s.removed += r.parse::<u64>().unwrap_or(0);
            s.files += 1;
        }
        s
    })
}

pub fn has_changes(wt: &Path, base_sha: &str) -> bool {
    stats(wt, base_sha).map(|s| s.files > 0).unwrap_or(false)
}

// ------------------------------------------------------------------ diff

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Hunk,
    Ctx,
    Add,
    Del,
    Note,
}

#[derive(Clone, Debug)]
pub struct DLine {
    pub kind: Kind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct DiffFile {
    pub path: String,
    /// "new", "deleted", "renamed from x", "" = modified
    pub note: String,
    pub added: u64,
    pub removed: u64,
    pub lines: Vec<DLine>,
}

#[derive(Clone, Debug, Default)]
pub struct Diff {
    pub files: Vec<DiffFile>,
    /// None = couldn't tell (old git), Some(vec![]) = merges cleanly, Some(files) = conflicts in these.
    pub conflicts: Option<Vec<String>>,
    /// What the conflict check compared against, e.g. "master".
    pub target: String,
}

/// Parse `git diff` output into files and numbered lines.
pub fn parse_diff(text: &str) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = vec![];
    let (mut old, mut new) = (0u32, 0u32);
    for raw in text.lines() {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            // "a/path b/path": take the b side (paths with spaces keep working since both halves match)
            let path = rest.rsplit_once(" b/").map(|(_, b)| b.to_string()).unwrap_or_else(|| rest.to_string());
            files.push(DiffFile { path, ..Default::default() });
            continue;
        }
        let Some(f) = files.last_mut() else { continue };
        if raw.starts_with("new file mode") {
            f.note = "new".into();
        } else if raw.starts_with("deleted file mode") {
            f.note = "deleted".into();
        } else if let Some(p) = raw.strip_prefix("rename from ") {
            f.note = format!("renamed from {p}");
        } else if let Some(p) = raw.strip_prefix("rename to ") {
            f.path = p.to_string();
        } else if raw.starts_with("Binary files ") {
            f.note = "binary".into();
            f.lines.push(DLine { kind: Kind::Note, old: None, new: None, text: "binary file changed".into() });
        } else if raw.starts_with("--- ") || raw.starts_with("+++ ") || raw.starts_with("index ") || raw.starts_with("similarity ") || raw.starts_with("old mode") || raw.starts_with("new mode") {
        } else if let Some(h) = raw.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@ context
            let mut parts = h.split_whitespace();
            let o = parts.next().unwrap_or("-0").trim_start_matches('-');
            let n = parts.next().unwrap_or("+0").trim_start_matches('+');
            old = o.split(',').next().and_then(|x| x.parse().ok()).unwrap_or(0);
            new = n.split(',').next().and_then(|x| x.parse().ok()).unwrap_or(0);
            let ctx = h.splitn(2, "@@").nth(1).unwrap_or("").trim().to_string();
            f.lines.push(DLine { kind: Kind::Hunk, old: Some(old), new: Some(new), text: ctx });
        } else if let Some(t) = raw.strip_prefix('+') {
            f.added += 1;
            f.lines.push(DLine { kind: Kind::Add, old: None, new: Some(new), text: t.to_string() });
            new += 1;
        } else if let Some(t) = raw.strip_prefix('-') {
            f.removed += 1;
            f.lines.push(DLine { kind: Kind::Del, old: Some(old), new: None, text: t.to_string() });
            old += 1;
        } else if let Some(t) = raw.strip_prefix(' ') {
            f.lines.push(DLine { kind: Kind::Ctx, old: Some(old), new: Some(new), text: t.to_string() });
            old += 1;
            new += 1;
        } else if raw.starts_with('\\') {
            // "\ No newline at end of file"
        } else if raw.is_empty() {
            f.lines.push(DLine { kind: Kind::Ctx, old: Some(old), new: Some(new), text: String::new() });
            old += 1;
            new += 1;
        }
    }
    files
}

/// The full diff of a task's worktree against its base, plus whether squash-merging it would conflict with
/// the main repo's current HEAD.
#[cfg(test)]
pub fn diff(repo: &Path, wt: &Path, base_sha: &str) -> Result<Diff, String> {
    diff_against(repo, wt, base_sha, None)
}

/// `diff`, with the conflict check against `target` (a branch) instead of the main repo's HEAD.
pub fn diff_against(repo: &Path, wt: &Path, base_sha: &str, target: Option<&str>) -> Result<Diff, String> {
    if !wt.is_dir() {
        return Err("the worktree is gone".into());
    }
    let (text, tree) = with_snapshot_index(wt, |env| {
        let d = run_env(wt, &["diff", "--cached", "--no-color", "--no-ext-diff", "-M", base_sha], env);
        let tree = run_env(wt, &["write-tree"], env);
        (d.stdout, if tree.ok { Some(tree.stdout.trim().to_string()) } else { None })
    })?;
    let files = parse_diff(&text);
    let (target, head_ref) = match target {
        Some(b) => (b.to_string(), format!("refs/heads/{b}")),
        None => (ok(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|_| "HEAD".into()), "HEAD".to_string()),
    };
    // a throwaway commit of the worktree's current state, so uncommitted work counts in the conflict check
    let conflicts = tree.and_then(|tree| {
        let c = ok(wt, &["commit-tree", &tree, "-p", base_sha, "-m", "oriel conflict check"]).ok()?;
        let head = ok(repo, &["rev-parse", &head_ref]).ok()?;
        let o = run(repo, &["merge-tree", "--write-tree", "--name-only", "--no-messages", &head, &c]);
        match o.code {
            0 => Some(vec![]),
            1 => Some(o.stdout.lines().skip(1).map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()),
            _ => None, // git older than 2.38
        }
    });
    Ok(Diff { files, conflicts, target })
}

// ------------------------------------------------------------------ merge / discard

/// Squash-merge a task into the main repo's checked-out branch and clean up. `progress` gets short status lines.
pub fn merge(repo: &Path, wt: &Path, branch: &str, base_branch: &str, title: &str, progress: &dyn Fn(&str)) -> Result<String, String> {
    let cur = ok(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if !base_branch.is_empty() && base_branch != "HEAD" && cur != base_branch {
        return Err(format!("the repo is on {cur}, but this task started from {base_branch} — check out {base_branch} first"));
    }
    let dirty = ok(repo, &["status", "--porcelain", "--untracked-files=no"])?;
    if !dirty.is_empty() {
        return Err(format!("{} has uncommitted changes — commit or stash them, then merge", repo.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()));
    }
    if wt.is_dir() {
        progress("committing leftovers…");
        let st = ok(wt, &["status", "--porcelain"])?;
        if !st.is_empty() {
            ok(wt, &["add", "-A"])?;
            ok(wt, &["commit", "-q", "--no-verify", "-m", title])?;
        }
    }
    progress("merging…");
    let o = run(repo, &["merge", "--squash", branch]);
    if !o.ok {
        let _ = run(repo, &["reset", "--merge"]);
        let msg = err_line(&o, &["merge"]);
        return Err(if o.stdout.contains("CONFLICT") || msg.contains("conflict") { "merge conflicts — nothing was merged; comment to have the agent rebase, or merge by hand".into() } else { msg });
    }
    let staged = !run(repo, &["diff", "--cached", "--quiet"]).ok;
    let msg = format!("{title} (oriel agent)");
    let summary = if staged {
        ok(repo, &["commit", "-q", "-m", &msg])?;
        let sha = ok(repo, &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
        format!("merged into {cur} as {sha}")
    } else {
        "nothing to merge (no changes)".to_string()
    };
    progress("cleaning up…");
    remove(repo, wt, branch)?;
    Ok(summary)
}

/// Remove a task's worktree and branch. Retries for a few seconds: on Windows the agent that just got closed
/// can still hold the folder open for a moment.
pub fn remove(repo: &Path, wt: &Path, branch: &str) -> Result<(), String> {
    let mut last = String::new();
    for i in 0..15 {
        if !wt.exists() {
            break;
        }
        let o = run(repo, &["worktree", "remove", "--force", &wt.to_string_lossy()]);
        if o.ok || !wt.exists() {
            break;
        }
        last = err_line(&o, &["worktree"]);
        if i == 14 {
            return Err(format!("couldn't remove the worktree: {last}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let _ = run(repo, &["worktree", "prune"]);
    if !branch.is_empty() && run(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).ok {
        ok(repo, &["branch", "-D", branch])?;
    }
    let _ = last;
    Ok(())
}

// ------------------------------------------------------------------ lead runs: the integration branch

pub struct RunStarted {
    pub branch: String,
    pub base_branch: String,
    pub base_sha: String,
    pub worktree: PathBuf,
}

/// A lead run's integration branch `oriel/lead-<slug>` at the current HEAD, plus the lead's own read-only
/// (detached) checkout of it at `wt`.
pub fn start_run(repo: &Path, wt: &Path, slug: &str) -> Result<RunStarted, String> {
    let base_sha = ok(repo, &["rev-parse", "HEAD"]).map_err(|_| "the repo has no commits yet — commit something first".to_string())?;
    let base_branch = ok(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|_| "HEAD".into());
    if base_branch == "HEAD" {
        return Err("the repo is on a detached HEAD — check out a branch first".into());
    }
    let branch = format!("oriel/lead-{slug}");
    if run(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).ok {
        return Err(format!("{branch} already exists"));
    }
    ok(repo, &["branch", &branch, &base_sha])?;
    if let Some(p) = wt.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("couldn't make {}: {e}", p.display()))?;
    }
    if wt.exists() {
        let _ = run(repo, &["worktree", "remove", "--force", &wt.to_string_lossy()]);
        let _ = std::fs::remove_dir_all(wt);
        let _ = run(repo, &["worktree", "prune"]);
    }
    if let Err(e) = ok(repo, &["worktree", "add", "--detach", &wt.to_string_lossy(), &base_sha]) {
        let _ = run(repo, &["branch", "-D", &branch]);
        return Err(e);
    }
    Ok(RunStarted { branch, base_branch, base_sha, worktree: wt.to_path_buf() })
}

/// Commit whatever the agent left uncommitted in its worktree. Returns whether anything was committed.
pub fn commit_leftovers(wt: &Path, title: &str) -> Result<bool, String> {
    if !wt.is_dir() {
        return Err("the worktree is gone".into());
    }
    let st = ok(wt, &["status", "--porcelain"])?;
    if st.is_empty() {
        return Ok(false);
    }
    ok(wt, &["add", "-A"])?;
    ok(wt, &["commit", "-q", "--no-verify", "-m", title])?;
    Ok(true)
}

fn merging(wt: &Path) -> bool {
    run(wt, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]).ok
}

/// Files that conflict when merging `tip` into `into` (both commits), or the merged tree when it's clean.
fn merge_tree(repo: &Path, into: &str, tip: &str) -> Result<Result<String, Vec<String>>, String> {
    let o = run(repo, &["merge-tree", "--write-tree", "--name-only", "--no-messages", into, tip]);
    match o.code {
        0 => Ok(Ok(o.stdout.lines().next().unwrap_or("").trim().to_string())),
        1 => Ok(Err(o.stdout.lines().skip(1).map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())),
        _ => Err(format!("conflict check failed (git 2.38+ needed): {}", err_line(&o, &["merge-tree"]))),
    }
}

#[derive(Debug, PartialEq)]
pub enum MergeErr {
    /// Nothing was merged: these files conflict with the integration branch.
    Conflicts(Vec<String>),
    /// Nothing was merged: the merged result failed the gate (the first lines of the failure).
    Gate(String),
    Failed(String),
}

/// What a task merge is and where it goes.
pub struct MergeJob<'a> {
    pub repo: &'a Path,
    pub wt: &'a Path,
    pub branch: &'a str,
    pub integration: &'a str,
    pub title: &'a str,
    pub worker: &'a str,
    pub task_id: &'a str,
    pub lead_wt: Option<&'a Path>,
}

/// Squash-merge a task into a lead run's integration branch without checking anything out: commit the
/// worktree's leftovers, 3-way merge its tip into the branch in memory (`git merge-tree`), make ONE candidate
/// commit (with an `Oriel-Task:` trailer), let `gate` test that exact tree, and only then move the branch to it.
/// On conflicts or a failed gate nothing changes. Afterwards the task's worktree + branch go and the lead's
/// checkout moves to the new tip. The caller serializes merges.
pub fn merge_into(j: &MergeJob, gate: &dyn Fn(&str) -> Result<(), String>) -> Result<String, MergeErr> {
    let MergeJob { repo, wt, branch, integration, title, worker, task_id, lead_wt } = *j;
    let f = MergeErr::Failed;
    if !wt.is_dir() {
        return Err(f("the task's worktree is gone".into()));
    }
    if merging(wt) {
        return Err(f("its worker is still resolving conflicts — wait for it".into()));
    }
    commit_leftovers(wt, title).map_err(f)?;
    let tip = ok(wt, &["rev-parse", "HEAD"]).map_err(f)?;
    let into = ok(repo, &["rev-parse", &format!("refs/heads/{integration}")]).map_err(|_| f(format!("the branch {integration} is gone")))?;
    let tree = match merge_tree(repo, &into, &tip).map_err(f)? {
        Ok(tree) => tree,
        Err(files) => return Err(MergeErr::Conflicts(files)),
    };
    let old_tree = ok(repo, &["rev-parse", &format!("{into}^{{tree}}")]).map_err(f)?;
    let summary = if tree == old_tree {
        "nothing to merge (no changes)".to_string()
    } else {
        let head = if worker.is_empty() { format!("{title} (oriel worker)") } else { format!("{title} (oriel worker {worker})") };
        let msg = format!("{head}\n\nOriel-Task: {task_id}");
        let c = ok(repo, &["commit-tree", &tree, "-p", &into, "-m", &msg]).map_err(f)?;
        gate(&c).map_err(MergeErr::Gate)?;
        // compare-and-swap: refuses if the branch moved under us
        ok(repo, &["update-ref", &format!("refs/heads/{integration}"), &c, &into]).map_err(f)?;
        if let Some(l) = lead_wt.filter(|l| l.is_dir()) {
            let _ = run(l, &["reset", "--hard", "-q", &c]);
        }
        let short = ok(repo, &["rev-parse", "--short", &c]).unwrap_or_default();
        let stat = ok(repo, &["diff", "--shortstat", &into, &c]).unwrap_or_default();
        format!("merged into {integration} as {short}{}", if stat.is_empty() { String::new() } else { format!(" ({})", stat.trim()) })
    };
    remove(repo, wt, branch).map_err(f)?;
    Ok(summary)
}

/// Check out `sha` (detached) in the run's gate worktree, making it first if needed. Untracked build output
/// (target/, node_modules/) stays, so gates build incrementally.
pub fn gate_checkout(repo: &Path, gate_wt: &Path, sha: &str) -> Result<(), String> {
    if !gate_wt.join(".git").exists() {
        if let Some(p) = gate_wt.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        let _ = std::fs::remove_dir_all(gate_wt);
        let _ = run(repo, &["worktree", "prune"]);
        ok(repo, &["worktree", "add", "--detach", &gate_wt.to_string_lossy(), sha])?;
        return Ok(());
    }
    ok(gate_wt, &["checkout", "-q", "-f", "--detach", sha]).map(|_| ())
}

/// The gate for a repo when the config doesn't name one: a quick compile check for the ecosystems where a
/// fresh checkout can build without an install step.
pub fn detect_gate(dir: &Path) -> Option<String> {
    if dir.join("Cargo.toml").is_file() {
        Some("cargo check --quiet".into())
    } else if dir.join("go.mod").is_file() {
        Some("go build ./...".into())
    } else {
        None
    }
}

/// The shell gates and acceptance commands run in: what agents write commands for. On Windows that's Git for
/// Windows' bash (the one Claude Code's Bash tool uses; never System32's WSL launcher), else PowerShell, else
/// cmd. Elsewhere `sh`. (program, args before the command, name for prompts)
pub fn gate_shell() -> &'static (PathBuf, Vec<String>, &'static str) {
    static SHELL: std::sync::OnceLock<(PathBuf, Vec<String>, &'static str)> = std::sync::OnceLock::new();
    SHELL.get_or_init(|| {
        if !cfg!(windows) {
            return (PathBuf::from("sh"), vec!["-c".into()], "sh");
        }
        let from_git = crate::config::which("git").and_then(|g| Some(g.parent()?.parent()?.join("bin").join("bash.exe")));
        let bash = [Some(PathBuf::from(r"C:\Program Files\Git\bin\bash.exe")), from_git].into_iter().flatten().find(|p| p.is_file());
        if let Some(b) = bash {
            return (b, vec!["-c".into()], "bash");
        }
        if let Some(p) = crate::config::which("pwsh") {
            return (p, vec!["-NoProfile".into(), "-NonInteractive".into(), "-Command".into()], "PowerShell");
        }
        (PathBuf::from("cmd.exe"), vec!["/d".into(), "/s".into(), "/c".into()], "cmd")
    })
}

/// Did a gate command fail to *run* (a shell syntax error, a missing program) rather than fail its check?
pub fn shell_trouble(msg: &str) -> bool {
    let m = msg.to_lowercase();
    ["is not recognized as", "command not found", "syntax error", "unexpected token", "was unexpected at this time", "missing `]'", "missing ']'", "unexpected eof", "unterminated", "parsererror", "no such file or directory: '"]
        .iter()
        .any(|p| m.contains(p))
        || m.contains("(exit 127)")
        || m.contains("(exit 9009)")
}

/// Run a gate command through the gate shell in `dir`, killed after `timeout`. Err = the first lines of the failure.
pub fn run_gate(dir: &Path, cmd: &str, timeout: std::time::Duration, env: &[(&str, &str)]) -> Result<(), String> {
    use std::io::Read;
    let (prog, pre, _) = gate_shell();
    let mut c = command(&prog.to_string_lossy());
    c.args(pre).arg(cmd);
    c.current_dir(dir).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).env("CI", "1").env_remove("NO_COLOR");
    for (k, v) in env {
        c.env(k, v);
    }
    let mut child = c.spawn().map_err(|e| format!("couldn't run the gate `{cmd}`: {e}"))?;
    let (mut so, mut se) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let ro = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = so.read_to_string(&mut s);
        s
    });
    let re = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = se.read_to_string(&mut s);
        s
    });
    let t0 = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if t0.elapsed() < timeout => std::thread::sleep(std::time::Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let out = format!("{}\n{}", ro.join().unwrap_or_default(), re.join().unwrap_or_default());
    match status {
        Some(s) if s.success() => Ok(()),
        None => Err(format!("`{cmd}` timed out after {}s", timeout.as_secs())),
        Some(s) => {
            let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
            // the first error and what follows it beats the tail (which is usually a summary)
            let start = lines.iter().position(|l| l.to_lowercase().contains("error") || l.contains("FAIL") || l.contains("panicked")).unwrap_or(lines.len().saturating_sub(20));
            let snippet = lines[start..].iter().take(20).map(|l| crate::ui::fit(l, 200)).collect::<Vec<_>>().join("\n");
            Err(format!("`{cmd}` failed (exit {}):\n{snippet}", s.code().unwrap_or(-1)))
        }
    }
}

/// Lines changed per file in a task (committed + uncommitted), for the lead's result cards.
pub fn file_stats(wt: &Path, base_sha: &str) -> Result<Vec<(String, u64, u64)>, String> {
    with_snapshot_index(wt, |env| {
        let o = run_env(wt, &["diff", "--cached", "--numstat", base_sha], env);
        o.stdout
            .lines()
            .filter_map(|l| {
                let mut it = l.split('\t');
                let (a, r, p) = (it.next()?, it.next()?, it.next()?);
                Some((p.to_string(), a.parse().unwrap_or(0), r.parse().unwrap_or(0)))
            })
            .collect()
    })
}

/// Would this task merge into `integration` right now? The files that conflict (empty = clean).
pub fn conflicts_with(repo: &Path, wt: &Path, base_sha: &str, integration: &str) -> Result<Vec<String>, String> {
    let d = diff_against(repo, wt, base_sha, Some(integration))?;
    d.conflicts.ok_or_else(|| "conflict check unavailable".into())
}

/// First half of resolve_conflicts: merge the integration branch into the task's worktree, leaving conflict
/// markers for its worker. Returns the conflicted files (empty = it merged cleanly and is committed).
pub fn start_resolve(wt: &Path, integration: &str, title: &str) -> Result<Vec<String>, String> {
    if merging(wt) {
        return Ok(ok(wt, &["diff", "--name-only", "--diff-filter=U"])?.lines().map(String::from).collect());
    }
    commit_leftovers(wt, title)?;
    let o = run(wt, &["merge", "--no-ff", "--no-commit", integration]);
    let files: Vec<String> = ok(wt, &["diff", "--name-only", "--diff-filter=U"])?.lines().map(String::from).collect();
    if files.is_empty() {
        if !o.ok && !merging(wt) {
            return Err(err_line(&o, &["merge"]));
        }
        if merging(wt) {
            ok(wt, &["commit", "-q", "--no-verify", "--no-edit"])?;
        }
    }
    Ok(files)
}

/// Second half: the worker is done — make sure no markers are left, then commit the merge.
pub fn finish_resolve(wt: &Path) -> Result<bool, String> {
    if !merging(wt) {
        return Ok(false);
    }
    let unmerged: Vec<String> = ok(wt, &["diff", "--name-only", "--diff-filter=U"])?.lines().map(String::from).collect();
    for f in &unmerged {
        let text = std::fs::read_to_string(wt.join(f)).unwrap_or_default();
        if text.lines().any(|l| l.starts_with("<<<<<<<") || l.starts_with(">>>>>>>")) {
            return Err(format!("{f} still has conflict markers"));
        }
    }
    ok(wt, &["add", "-A"])?;
    ok(wt, &["commit", "-q", "--no-verify", "--no-edit"])?;
    Ok(true)
}

/// A task's changes as unified diff text (committed + uncommitted + new files), optionally one path, in pages
/// of `page_lines` (page 1 = the first). The stat heads page 1.
pub fn diff_text(wt: &Path, base_sha: &str, path: &str, page: usize, page_lines: usize) -> Result<String, String> {
    if !wt.is_dir() {
        return Err("the worktree is gone".into());
    }
    let (stat, text) = with_snapshot_index(wt, |env| {
        let mut sa = vec!["diff", "--cached", "--stat=100", base_sha];
        let mut da = vec!["diff", "--cached", "--no-color", "--no-ext-diff", "-M", base_sha];
        if !path.is_empty() {
            sa.extend(["--", path]);
            da.extend(["--", path]);
        }
        (run_env(wt, &sa, env).stdout, run_env(wt, &da, env).stdout)
    })?;
    if text.trim().is_empty() {
        return Ok(if path.is_empty() { "(no changes)".into() } else { format!("(no changes in {path})") });
    }
    let lines: Vec<&str> = text.lines().collect();
    let per = page_lines.max(20);
    let pages = lines.len().div_ceil(per);
    let page = page.clamp(1, pages.max(1));
    let mut out = String::new();
    if page == 1 {
        out.push_str(stat.trim_end());
        out.push_str("\n\n");
    }
    out.push_str(&lines[(page - 1) * per..(page * per).min(lines.len())].join("\n"));
    if pages > 1 {
        out.push_str(&format!("\n— page {page} of {pages}{}", if page < pages { format!("; ask for page {} for more", page + 1) } else { String::new() }));
    }
    Ok(out)
}

/// The whole run for the user's review: the integration branch against where it started, plus whether it would
/// squash-merge cleanly into the repo's checked-out branch.
pub fn branch_diff(repo: &Path, base_sha: &str, branch: &str) -> Result<Diff, String> {
    let tip = ok(repo, &["rev-parse", &format!("refs/heads/{branch}")]).map_err(|_| format!("the branch {branch} is gone"))?;
    let d = run(repo, &["diff", "--no-color", "--no-ext-diff", "-M", base_sha, &tip]);
    if !d.ok {
        return Err(err_line(&d, &["diff"]));
    }
    let files = parse_diff(&d.stdout);
    let target = ok(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|_| "HEAD".into());
    let conflicts = ok(repo, &["rev-parse", "HEAD"]).ok().and_then(|head| merge_tree(repo, &head, &tip).ok()).map(|r| r.err().unwrap_or_default());
    Ok(Diff { files, conflicts, target })
}
