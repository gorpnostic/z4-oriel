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
pub fn start(repo: &Path, wt: &Path, slug: &str, task_id: &str, hook_exe: Option<&Path>) -> Result<Started, String> {
    let base_sha = ok(repo, &["rev-parse", "HEAD"]).map_err(|_| "the repo has no commits yet — commit something first".to_string())?;
    let base_branch = ok(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|_| "HEAD".into());
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
pub fn diff(repo: &Path, wt: &Path, base_sha: &str) -> Result<Diff, String> {
    if !wt.is_dir() {
        return Err("the worktree is gone".into());
    }
    let (text, tree) = with_snapshot_index(wt, |env| {
        let d = run_env(wt, &["diff", "--cached", "--no-color", "--no-ext-diff", "-M", base_sha], env);
        let tree = run_env(wt, &["write-tree"], env);
        (d.stdout, if tree.ok { Some(tree.stdout.trim().to_string()) } else { None })
    })?;
    let files = parse_diff(&text);
    let target = ok(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|_| "HEAD".into());
    // a throwaway commit of the worktree's current state, so uncommitted work counts in the conflict check
    let conflicts = tree.and_then(|tree| {
        let c = ok(wt, &["commit-tree", &tree, "-p", base_sha, "-m", "oriel conflict check"]).ok()?;
        let head = ok(repo, &["rev-parse", "HEAD"]).ok()?;
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
