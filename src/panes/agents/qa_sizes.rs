//! QA sizes for the agents board (see crate::qa_sizes): the demo board from the agents tests plus cards with
//! awkward titles in every column, in each mode the board can be in (board, new task, repo picker, lead
//! form, roster, planner prompt, diff). Everything lives in a scratch folder; nothing is started.

use super::*;
use crate::qa_sizes::{AWKWARD, Known, scratch, sweep, verdict};
use crate::testkit::Kit;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::Path;

fn board(dir: &Path) -> Agents {
    let mut p = super::tests::demo(dir);
    let repo = p.repo_key();
    let now = store::now();
    let statuses = [Status::Todo, Status::Running, Status::Blocked, Status::Review, Status::Done];
    for (i, t) in AWKWARD.iter().cycle().take(20).enumerate() {
        p.store.tasks.push(Task {
            id: format!("qa{i}"),
            repo: repo.clone(),
            title: t.to_string(),
            prompt: AWKWARD.join(" "),
            agent: ["claude", "codex", "kimi"][i % 3].into(),
            model: AWKWARD[4].into(),
            status: statuses[i % statuses.len()],
            outcome: if i % 2 == 0 { "merged".into() } else { "discarded".into() },
            branch: format!("oriel/{}", AWKWARD[4]),
            base_branch: AWKWARD[1].into(),
            last: AWKWARD[(i + 1) % AWKWARD.len()].into(),
            question: AWKWARD[(i + 2) % AWKWARD.len()].into(),
            error: AWKWARD[(i + 3) % AWKWARD.len()].into(),
            cost_usd: 99_999.99,
            tokens: 9_999_999_999,
            added: 123_456,
            removed: 654_321,
            files: 9_999,
            created: now - 90_000,
            started: now - 80_000,
            finished: now - 10,
            followups: 42,
            ..Default::default()
        });
    }
    p.store.repos.push(format!("C:\\code\\{}", AWKWARD[1]));
    p.store.repos.push(format!("C:\\code\\{}", AWKWARD[0]));
    p
}

/// The board with `key` pressed (opens a mode), or with a diff open.
fn in_mode(k: &Kit, dir: &Path, key: Option<char>, diff: bool) -> Agents {
    let mut p = board(dir);
    if let Some(c) = key {
        let mut actions = vec![];
        let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut actions, focused: true, time: 1.0 };
        p.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &mut cx);
    }
    if diff {
        let long = AWKWARD.join(" ");
        let text = format!(
            "diff --git a/src/{f}.rs b/src/{f}.rs\nindex 1..2 100644\n--- a/src/{f}.rs\n+++ b/src/{f}.rs\n@@ -1,3 +1,4 @@ {long}\n context {long}\n-old\t{long}\n+new 日本語 {long}\n+\t\ttabs\n \n",
            f = AWKWARD[1]
        );
        let files = git::parse_diff(&text.repeat(3));
        p.mode = Mode::Diff(DiffView { id: p.store.tasks[4].id.clone(), data: Some(Ok(git::Diff { files, conflicts: Some(vec![AWKWARD[0].into(), AWKWARD[1].into()]), target: AWKWARD[1].into() })), file: 0, scroll: 0 });
    }
    p
}

/// For the whole-app sweep: the agents tab with a full board, in a scratch folder.
pub(crate) fn app_pane() -> Box<dyn crate::pane::Pane> {
    Box::new(board(&scratch("agents-app")))
}

/// Bugs found by this sweep (each has its own ignored test below).
const KNOWN: &[Known] = &[];

#[test]
fn qa_sizes_agents_board() {
    let mut k = Kit::new();
    let mut bad = vec![];
    for (label, key, diff) in [("board", None, false), ("new task", Some('n'), false), ("repo picker", Some('o'), false), ("lead form", Some('L'), false), ("roster", Some('R'), false), ("planner", Some('P'), false), ("diff", None, true)] {
        let dir = scratch(&format!("agents-{}", label.replace(' ', "-")));
        // only the board gets navigation keys: in a form they'd be typed into fields
        let keys = label == "board" || label == "diff";
        bad.extend(sweep(&format!("agents({label})"), &mut k, &mut |k| Box::new(in_mode(k, &dir, key, diff)), keys, 0));
    }
    verdict(bad, KNOWN);
}
