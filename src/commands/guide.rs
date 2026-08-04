use std::ffi::OsStr;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::{env, fs};

use anstyle::Style;
use anyhow::{Context, Result, bail};
use console::{Alignment, Key, Term, pad_str, truncate_str};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Select};

use crate::commands::Run;
use crate::style;

type Walk = fn(&mut Tour) -> Result<()>;

/// The available tours: (topic, menu description, runner).
const TOPICS: &[(&str, &str, Walk)] = &[
    ("intro", "create, submit, restack, and land a stack", intro),
    (
        "conflicts",
        "when a restack stops: resolve, continue, abort",
        conflicts,
    ),
    ("repair", "rebuild lost stack metadata", repair),
    (
        "absorb",
        "fold review fixes back into the commits they belong to",
        absorb,
    ),
    (
        "adopt",
        "adopt a branch into a stack, or move it to a new parent",
        adopt,
    ),
    (
        "split",
        "split a branch's commits into a stack of branches",
        split,
    ),
    ("undo", "reverse the last stack-rewriting command", undo),
    (
        "worktrees",
        "a branch per worktree: the flows and the gotchas",
        worktrees,
    ),
];

/// Walk the stacked workflow in a disposable sandbox repository.
#[derive(Debug, clap::Args)]
pub struct Guide {
    /// Which tour to run; omit for a menu.
    #[arg(value_parser = clap::builder::PossibleValuesParser::new(["intro", "conflicts", "repair", "absorb", "adopt", "split", "undo", "worktrees"]))]
    topic: Option<String>,
}

impl Run for Guide {
    fn run(self) -> Result<()> {
        guide(self.topic.as_deref())
    }
}

fn guide(topic: Option<&str>) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("the guide is interactive; run it from a terminal");
    }

    banner("git stk guide");
    say("Short interactive tours. Everything happens in a disposable sandbox");
    say("repository - your real work is never touched, and a built-in demo");
    say("provider stands in for GitHub: same commands, no network.");
    say("Each step opens full-screen; scroll with j/k or the arrows, Enter to");
    say("move on, q to quit.");
    println!();

    let chosen = match topic {
        Some(topic) => TOPICS
            .iter()
            .find(|(name, _, _)| *name == topic)
            .context("unknown guide topic")?,
        None => {
            let items: Vec<String> = TOPICS
                .iter()
                .map(|(name, blurb, _)| format!("{name} - {blurb}"))
                .collect();
            let index = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("which tour?")
                .items(&items)
                .default(0)
                .interact()
                .context("nothing chosen")?;
            &TOPICS[index]
        }
    };
    println!();

    let sandbox = env::temp_dir().join(format!("git-stk-guide-{}", std::process::id()));
    if sandbox.exists() {
        fs::remove_dir_all(&sandbox).context("failed to clear an old sandbox")?;
    }
    say(&format!("sandbox: {}", sandbox.display()));
    println!();
    setup_sandbox(&sandbox)?;

    let mut tour = Tour::new(&sandbox, chosen.0);
    let finished = (chosen.2)(&mut tour);

    // Hand the sandbox over or clean it up, whether or not the tour ran dry.
    let delete = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("delete the sandbox?")
        .default(true)
        .interact()
        .unwrap_or(true);
    // The worktrees tour creates linked worktrees, which live beside the
    // sandbox rather than inside it - so removing the sandbox alone would leave
    // them behind.
    let worktrees = sibling_worktree_dir(&sandbox);
    if delete {
        fs::remove_dir_all(&sandbox).context("failed to remove the sandbox")?;
        if worktrees.exists() {
            fs::remove_dir_all(&worktrees).context("failed to remove the sandbox worktrees")?;
        }
        say("sandbox removed");
    } else {
        say(&format!("kept: cd {}", sandbox.display()));
        if worktrees.exists() {
            say(&format!("its worktrees: {}", worktrees.display()));
        }
        say("it uses `git config stk.provider demo`, so every command works offline");
    }

    finished
}

fn intro(tour: &mut Tour) -> Result<()> {
    tour.banner("1/5 - a stack is just branches");
    tour.say("Each branch carries one reviewable change and knows its parent.");
    tour.say("`new` creates a child of wherever you stand:");
    tour.stk(&["new", "feature/login"])?;
    tour.commit("login.txt", "username + password form\n", "add login form")?;
    tour.stk(&["new", "feature/avatar"])?;
    tour.commit("avatar.txt", "round avatars\n", "add avatars")?;
    tour.say("Two branches, stacked. `list` draws the pile, trunk at the bottom:");
    tour.stk(&["list"])?;
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("2/5 - submit the whole stack");
    tour.say("One command opens (or updates) a review per branch, parent-first,");
    tour.say("and writes a live stack overview into every description:");
    tour.stk(&["submit", "--stack"])?;
    tour.stk(&["status"])?;
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("3/5 - parents move; restack follows");
    tour.say("Review feedback lands on the bottom branch:");
    tour.stk(&["down"])?;
    tour.commit(
        "login.txt",
        "username + password form\nremember me\n",
        "add remember me",
    )?;
    tour.say("The child is now behind its parent - `list` notices:");
    tour.stk(&["list"])?;
    tour.say("`restack` rebases every descendant back onto its parent:");
    tour.stk(&["restack"])?;
    tour.stk(&["top"])?;
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("4/5 - land the stack");
    tour.say("`merge --all` repeats merge-bottom-then-sync until the stack is");
    tour.say("complete: children retarget, merged branches vanish, the overview");
    tour.say("in every review restyles as history accumulates:");
    tour.stk(&["merge", "--all", "-y"])?;
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("5/5 - nothing left but trunk");
    tour.stk(&["list"])?;
    tour.say("That is the whole loop: new -> commit -> submit -> merge.");
    tour.say("On a real repo the provider is detected from your remote; day to day");
    tour.say("you mostly run `git stk new`, `git stk submit --stack`, and");
    tour.say("`git stk merge --all`. `git stk status` and the hints fill the gaps.");
    tour.finish()
}

fn conflicts(tour: &mut Tour) -> Result<()> {
    tour.banner("1/3 - set up a collision");
    tour.say("A two-branch stack where both branches touch the same line:");
    tour.stk(&["new", "feature/payment"])?;
    tour.commit("notes.txt", "use stripe\n", "choose payment provider")?;
    tour.stk(&["new", "feature/receipts"])?;
    tour.commit("notes.txt", "use stripe with receipts\n", "email receipts")?;
    tour.say("Now the parent changes its mind about that very line:");
    tour.stk(&["down"])?;
    tour.commit("notes.txt", "use paypal\n", "switch to paypal")?;
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("2/3 - the restack stops, with context");
    tour.say("Replaying the child onto the rewritten parent cannot succeed; the");
    tour.say("restack stops, shows git's conflict output, and says what to do:");
    tour.stk_fails(&["restack"])?;
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("3/3 - resolve, then continue");
    tour.say("Fix the file and stage it, exactly like any rebase conflict:");
    tour.edit_and_add("notes.txt", "use paypal with receipts\n")?;
    tour.say("`continue` picks the restack back up where it stopped");
    tour.say("(`git stk abort` would have unwound it instead):");
    tour.stk(&["continue"])?;
    tour.stk(&["list"])?;
    tour.say("Conflicts interrupt the restack, never break it: resolve, continue,");
    tour.say("and the rest of the stack follows.");
    tour.finish()
}

fn repair(tour: &mut Tour) -> Result<()> {
    tour.banner("1/3 - a healthy stack");
    tour.stk(&["new", "feature/api"])?;
    tour.commit("api.txt", "endpoints\n", "add api")?;
    tour.stk(&["new", "feature/ui"])?;
    tour.commit("ui.txt", "buttons\n", "add ui")?;
    tour.stk(&["submit", "--stack"])?;
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("2/3 - the metadata vanishes");
    tour.say("Stack parents are plain `branch.<name>.stkParent` entries in");
    tour.say(".git/config - annotations, not state. Suppose one gets lost:");
    tour.note("git config --unset branch.feature/ui.stkParent");
    run_git(
        tour.sandbox,
        &["config", "--unset", "branch.feature/ui.stkParent"],
    )?;
    tour.say("The stack no longer knows feature/ui belongs to it:");
    tour.stk(&["list"])?;
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("3/3 - repair rebuilds it");
    tour.say("`repair` re-derives parents from review bases (when a provider is");
    tour.say("reachable) and branch ancestry, and verifies recorded fork points:");
    tour.stk(&["repair", "--dry-run"])?;
    tour.stk(&["repair"])?;
    tour.stk(&["list"])?;
    tour.say("Branches are the real state; metadata is always recoverable.");
    tour.say("Anything repair cannot resolve safely, it reports for a manual");
    tour.say("`git stk adopt`.");
    tour.finish()
}

fn absorb(tour: &mut Tour) -> Result<()> {
    tour.banner("1/3 - fixes scattered across the stack");
    tour.say("A two-branch stack, each branch owning one file:");
    tour.stk(&["new", "feature/login"])?;
    tour.commit("login.txt", "username + password form\n", "add login form")?;
    tour.stk(&["new", "feature/avatar"])?;
    tour.commit("avatar.txt", "round avatars\n", "add avatars")?;
    tour.say("Review comes back: two small fixes, one on each branch's file.");
    tour.say("You make both edits from the top and stage them, as usual:");
    tour.edit_and_add("login.txt", "username + password form, with 2FA\n")?;
    tour.edit_and_add("avatar.txt", "round avatars, lazy-loaded\n")?;
    tour.say("Both fixes sit staged together, but each belongs to a different commit");
    tour.say("further down the stack:");
    tour.stk(&["status"])?;
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("2/3 - preview where each hunk lands");
    tour.say("`absorb` blames every staged hunk and routes it to the commit that");
    tour.say("introduced the lines it touches. `--dry-run` shows the plan first:");
    tour.stk(&["absorb", "--dry-run"])?;
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("3/3 - fold them in");
    tour.say("Run it for real: each fix becomes a `fixup!` of its owning commit, an");
    tour.say("autosquash rebase folds them in, and every branch ref rides along:");
    tour.stk(&["absorb"])?;
    tour.say("The history reads as if the fixes were always there - no extra commits:");
    tour.show_git(
        "git log --oneline main..feature/avatar",
        &[
            "--no-pager",
            "-c",
            "color.ui=always",
            "log",
            "--oneline",
            "main..feature/avatar",
        ],
    )?;
    tour.say("Hunks that cannot be attributed - brand-new lines, trunk-owned lines, a");
    tour.say("hunk spanning two commits - are left staged and reported, never guessed.");
    tour.finish()
}

fn adopt(tour: &mut Tour) -> Result<()> {
    tour.banner("1/3 - adopt a hand-made branch");
    tour.say("Not every branch begins with `git stk new`. Suppose you branched off");
    tour.say("the trunk by hand and did some work:");
    tour.note("git switch -c feature/logging");
    run_git(tour.sandbox, &["switch", "-c", "feature/logging"])?;
    tour.commit("logging.txt", "structured logs\n", "add logging")?;
    tour.say("git-stk has no metadata for it yet. `adopt` records its parent -");
    tour.say("metadata only, nothing is rewritten - folding it into a stack:");
    tour.stk(&["adopt", "--parent", "main"])?;
    tour.stk(&["list"])?;
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("2/3 - move a branch onto another");
    tour.say("Two branches, each started independently off the trunk:");
    tour.note("git switch main");
    run_git(tour.sandbox, &["switch", "main"])?;
    tour.stk(&["new", "feature/api"])?;
    tour.commit("api.txt", "endpoints\n", "add api")?;
    tour.note("git switch main");
    run_git(tour.sandbox, &["switch", "main"])?;
    tour.stk(&["new", "feature/web"])?;
    tour.commit("web.txt", "pages\n", "add web")?;
    tour.say("`list` shows them as siblings on the trunk:");
    tour.stk(&["list", "--all"])?;
    tour.say("But feature/web really belongs on top of feature/api. Re-point its");
    tour.say("parent with `adopt`, then `restack` replays its commits onto the new");
    tour.say("base (only its own commits move; the parent's are already there):");
    tour.stk(&["adopt", "--parent", "feature/api"])?;
    tour.stk(&["restack"])?;
    tour.stk(&["list"])?;
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("3/3 - detach: the inverse");
    tour.say("`detach` drops a branch's stack metadata, leaving the branch and its");
    tour.say("commits untouched - handy when something was adopted by mistake:");
    tour.stk(&["detach", "feature/web"])?;
    tour.stk(&["list", "--all"])?;
    tour.say("feature/web still exists; git-stk just no longer tracks it. Re-`adopt`");
    tour.say("it onto any parent whenever you want it back in a stack.");
    tour.finish()
}

fn worktrees(tour: &mut Tour) -> Result<()> {
    tour.banner("1/4 - a branch in a worktree of its own");
    tour.say("A git worktree is a second checkout of the same repository, on its");
    tour.say("own branch. `new --worktree` makes one instead of checking the branch");
    tour.say("out here - so you keep whatever you were doing:");
    tour.stk(&["new", "feature/api"])?;
    tour.commit("api.txt", "endpoints\n", "add api")?;
    tour.stk(&["new", "feature/web", "--worktree"])?;
    tour.say("Note what did *not* happen: we are still on feature/api. The new");
    tour.say("branch lives somewhere else entirely.");
    tour.show_git("git branch --show-current", &["branch", "--show-current"])?;
    tour.say("`list` says where each branch lives, so the stack is still one view:");
    tour.stk(&["list", "--local"])?;
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("2/4 - gotcha: a branch can only be checked out once");
    tour.say("Git refuses to check out a branch a second worktree already holds.");
    tour.say("That is the central worktree gotcha, and it makes `up` impossible -");
    tour.say("moving up the stack is now a `cd`, not a checkout:");
    tour.stk_fails(&["up"])?;
    tour.say("So navigation can print where to go instead, and let the shell move:");
    tour.note("cd \"$(git stk up --from-path)\"");
    tour.say("Put that in a shell function and `up`/`down`/`top`/`bottom` follow the");
    tour.say("branch wherever it lives - into a worktree, or an ordinary checkout");
    tour.say("when it is here. The README has the snippet.");
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("3/4 - gotcha: rewrites refuse up front");
    tour.say("Git will not rebase a branch another worktree holds either. So when a");
    tour.say("parent moves and a held branch would need replaying:");
    tour.commit("api.txt", "endpoints\nauth\n", "add auth")?;
    tour.say("`restack` refuses before touching anything, rather than rewriting half");
    tour.say("the stack and failing in the middle:");
    tour.stk_fails(&["restack"])?;
    tour.say("Nothing was rewritten. Hand the branch back with");
    tour.say("`git -C <path> checkout --detach`, then try again - that works even when");
    tour.say("the holder is your main worktree, which cannot be removed. `undo` and");
    tour.say("`cleanup` are just as");
    tour.say("careful: undo refuses rather than rewinding a branch under a worktree");
    tour.say("that would not notice, and cleanup keeps such a branch and moves on");
    tour.say("instead of failing the whole run.");
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("4/4 - run, and who owns a worktree");
    tour.say("`run` uses a throwaway worktree of its own, so checking the stack");
    tour.say("never moves your HEAD and never needs a clean tree:");
    tour.edit_and_add("api.txt", "endpoints\nauth\nwork in progress\n")?;
    tour.say("Uncommitted work, and `run` still walks every branch:");
    tour.stk(&["run", "--", "git", "log", "--oneline", "-1"])?;
    tour.say("The catch: a fresh worktree has no untracked build output - no");
    tour.say("node_modules, no target/ - so a command that needs those may fail");
    tour.say("there having passed in your checkout. `--no-worktree` runs the old way.");
    tour.say("");
    tour.say("Finally, ownership. git-stk removes the worktrees *it* created (from");
    tour.say("`new --worktree`) once their branch lands, and never touches one you");
    tour.say("made by hand - nor an owned one with uncommitted work still in it.");
    tour.finish()
}

fn undo(tour: &mut Tour) -> Result<()> {
    tour.banner("1/2 - rewrite the stack");
    tour.say("Stack-rewriting commands snapshot the stack before they touch it,");
    tour.say("so git stk undo can put it back. Start with two branches:");
    tour.stk(&["new", "feature/api"])?;
    tour.commit("api.txt", "endpoints\n", "add api")?;
    tour.stk(&["new", "feature/web"])?;
    tour.commit("web.txt", "pages\n", "add web")?;
    tour.say("Feedback lands on the bottom branch, leaving the child behind it:");
    tour.stk(&["down"])?;
    tour.commit("api.txt", "endpoints\nauth\n", "add auth")?;
    tour.say("`restack` replays feature/web onto the rewritten feature/api - a real");
    tour.say("rewrite of its commit:");
    tour.stk(&["restack"])?;
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("2/2 - take it back");
    tour.say("Changed your mind? `undo` reverses the last stack-rewriting command,");
    tour.say("restoring every branch tip and the stack metadata from that snapshot:");
    tour.stk(&["undo"])?;
    tour.say("feature/web is back exactly where it was. `undo` is one level deep and");
    tour.say("one-shot - a second one has nothing left to restore:");
    tour.stk_fails(&["undo"])?;
    tour.say("And it is local only: pushes and already-merged reviews are never");
    tour.say("reverted - `undo` touches branch tips and metadata, nothing remote.");
    tour.finish()
}

fn split(tour: &mut Tour) -> Result<()> {
    tour.banner("1/3 - a pile of commits on one branch");
    tour.say("Sometimes you commit several changes on one branch before carving");
    tour.say("them into reviewable pieces. Start with three commits:");
    tour.stk(&["new", "feature/checkout"])?;
    tour.commit("cart.txt", "cart model\n", "add cart model")?;
    tour.commit("payment.txt", "stripe integration\n", "add payment")?;
    tour.commit("receipt.txt", "email receipt\n", "send receipt")?;
    tour.say("`list --commits` nests each branch's own commits, newest first, so");
    tour.say("you can see the boundaries before splitting:");
    tour.stk(&["list", "--commits"])?;
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("2/3 - split into a stack");
    tour.say("`split --per-commit` makes one branch per commit, bottom-up, named");
    tour.say("from each subject. The original branch stays as the leaf, and nothing");
    tour.say("is rewritten - the new branches just point at the existing commits:");
    tour.stk(&["split", "--per-commit"])?;
    tour.stk(&["list"])?;
    tour.say("(Plain `git stk split` opens an interactive picker instead, to group");
    tour.say("several commits into one branch.)");
    if tour.pause()?.stop() {
        return Ok(());
    }

    tour.banner("3/3 - tidy a name");
    tour.say("Auto-slugged names are a fine start; `rename` cleans one up and keeps");
    tour.say("the stack intact - children pointing at the old name are retargeted:");
    tour.stk(&["rename", "add-cart-model", "feature/cart"])?;
    tour.stk(&["list"])?;
    tour.say("From here, `git stk submit --stack` opens one review per branch, in");
    tour.say("order - the same as any other stack.");
    tour.finish()
}

/// One full-screen step: a pinned title, a scrollable body of narration and
/// captured command output, and a footer of scroll hints. The tour functions
/// build a screen with `banner`/`say`/`stk`/..., then `pause` (or `finish`)
/// renders it and waits for the reader.
struct Tour<'a> {
    sandbox: &'a Path,
    topic: &'a str,
    term: Term,
    title: String,
    lines: Vec<String>,
}

/// What the reader chose at a `pause`: move on, or quit the tour.
enum Flow {
    Continue,
    Stop,
}

impl Flow {
    fn stop(&self) -> bool {
        matches!(self, Self::Stop)
    }
}

impl<'a> Tour<'a> {
    fn new(sandbox: &'a Path, topic: &'a str) -> Self {
        Self {
            sandbox,
            topic,
            term: Term::stdout(),
            title: String::new(),
            lines: Vec::new(),
        }
    }

    /// Start a fresh screen with `title`. Does not render: content accrues
    /// until the next `pause`/`finish`.
    fn banner(&mut self, title: &str) {
        self.title = title.to_owned();
        self.lines.clear();
    }

    /// A line of narration.
    fn say(&mut self, line: &str) {
        self.lines.push(style::dim(line));
    }

    /// A shell-prompt line for a step we narrate but do not capture output
    /// from (e.g. a manual `git config --unset`).
    fn note(&mut self, command: &str) {
        self.lines.push(format!("{} {command}", style::dim("$")));
    }

    /// Run `git stk <args>` in the sandbox, showing the command and its
    /// output. Fails if the command does.
    fn stk(&mut self, args: &[&str]) -> Result<()> {
        let output = self.run_stk(args)?;
        if !output.status.success() {
            bail!("`git stk {}` failed in the sandbox", args.join(" "));
        }
        Ok(())
    }

    /// Like `stk`, for the step that is supposed to stop (the conflict).
    fn stk_fails(&mut self, args: &[&str]) -> Result<()> {
        let output = self.run_stk(args)?;
        if output.status.success() {
            bail!(
                "`git stk {}` was expected to stop on the conflict",
                args.join(" ")
            );
        }
        Ok(())
    }

    fn run_stk(&mut self, args: &[&str]) -> Result<Output> {
        self.note(&format!("git stk {}", args.join(" ")));
        let binary = env::current_exe().context("failed to locate the running binary")?;
        let output = capture(self.sandbox, &binary, args)?;
        self.absorb_output(&output);
        Ok(output)
    }

    /// Run a raw `git` command and show it under `display` with its output.
    fn show_git(&mut self, display: &str, args: &[&str]) -> Result<()> {
        self.note(display);
        let output = capture(self.sandbox, OsStr::new("git"), args)?;
        self.absorb_output(&output);
        if !output.status.success() {
            bail!("`{display}` failed in the sandbox");
        }
        Ok(())
    }

    /// Write `contents` to `file` and commit it, narrating the edit.
    fn commit(&mut self, file: &str, contents: &str, message: &str) -> Result<()> {
        self.note(&format!("edit {file}, then git commit -m {message:?}"));
        fs::write(self.sandbox.join(file), contents).context("failed to write sandbox file")?;
        run_git(self.sandbox, &["add", file])?;
        run_git(self.sandbox, &["commit", "-q", "-m", message])
    }

    /// Write `contents` to `file` and stage it without committing - a review
    /// fix, or a resolved conflict.
    fn edit_and_add(&mut self, file: &str, contents: &str) -> Result<()> {
        self.note(&format!("edit {file}, then git add {file}"));
        fs::write(self.sandbox.join(file), contents).context("failed to write sandbox file")?;
        run_git(self.sandbox, &["add", file])
    }

    /// Append a captured command's output, then a blank separator line.
    fn absorb_output(&mut self, output: &Output) {
        for stream in [&output.stdout, &output.stderr] {
            let text = String::from_utf8_lossy(stream);
            let text = text.trim_end_matches(['\n', '\r']);
            if text.is_empty() {
                continue;
            }
            for line in text.split('\n') {
                self.lines.push(line.trim_end_matches('\r').to_owned());
            }
        }
        self.lines.push(String::new());
    }

    /// Render the current screen and wait for the reader to move on or quit.
    fn pause(&mut self) -> Result<Flow> {
        self.present("j/k/up/down scroll - space/pgdn page - enter continue - q quit")
    }

    /// Render the final screen; enter or q both end the tour.
    fn finish(&mut self) -> Result<()> {
        self.present("j/k/up/down scroll - enter/q to finish")?;
        Ok(())
    }

    /// The pager: draw the framed screen and scroll it until the reader
    /// presses enter (continue) or q/esc (stop).
    fn present(&mut self, hint: &str) -> Result<Flow> {
        self.term.hide_cursor().ok();
        self.term.clear_screen().ok();

        let mut scroll = 0usize;
        let mut read_errors = 0u32;
        let flow = loop {
            let (rows, cols) = self.term.size();
            let (rows, cols) = (rows as usize, cols as usize);
            let body = rows.saturating_sub(2).max(1);
            let max_scroll = self.lines.len().saturating_sub(body);
            scroll = scroll.min(max_scroll);
            self.draw(scroll, cols, body, hint);

            match self.term.read_key() {
                Ok(key) => {
                    read_errors = 0;
                    match key {
                        Key::ArrowDown | Key::Char('j') => scroll = (scroll + 1).min(max_scroll),
                        Key::ArrowUp | Key::Char('k') => scroll = scroll.saturating_sub(1),
                        Key::PageDown | Key::Char(' ') => scroll = (scroll + body).min(max_scroll),
                        Key::PageUp => scroll = scroll.saturating_sub(body),
                        Key::Home | Key::Char('g') => scroll = 0,
                        Key::End | Key::Char('G') => scroll = max_scroll,
                        Key::Enter => break Flow::Continue,
                        Key::Char('q') | Key::Escape | Key::CtrlC => break Flow::Stop,
                        _ => {}
                    }
                }
                // A transient read error (e.g. a resize interrupting the read)
                // should redraw and retry, not end the tour - but bail if they
                // keep coming so a vanished terminal can't spin forever.
                Err(_) => {
                    read_errors += 1;
                    if read_errors >= 3 {
                        break Flow::Stop;
                    }
                }
            }
        };

        self.term.show_cursor().ok();
        self.term.clear_screen().ok();
        Ok(flow)
    }

    /// Compose and paint one frame: header bar, `body` rows of content from
    /// `scroll`, and a footer bar. Every row is exactly `cols` wide so each
    /// frame fully overwrites the last.
    fn draw(&self, scroll: usize, cols: usize, body: usize, hint: &str) {
        let bar = Style::new().invert();
        let header = format!("{} - {}", self.topic, self.title);
        let mut frame = style::paint(bar, &fit(&format!(" {header}"), cols));

        for row in 0..body {
            frame.push('\n');
            let line = self.lines.get(scroll + row).map_or("", String::as_str);
            frame.push_str(&fit(line, cols));
        }

        let scrollable = self.lines.len() > body;
        let footer = if scrollable {
            format!(
                " {hint}   [{}/{}]",
                (scroll + body).min(self.lines.len()),
                self.lines.len()
            )
        } else {
            format!(" {hint}")
        };
        frame.push('\n');
        frame.push_str(&style::paint(bar, &fit(&footer, cols)));

        // Best effort: a failed frame must not abort the loop, or the cursor
        // restore at the end of `present` would be skipped.
        self.term.move_cursor_to(0, 0).ok();
        print!("{frame}");
        let _ = std::io::stdout().flush();
    }
}

/// Truncate (ANSI-aware) to `width`, then pad with spaces to exactly `width`.
fn fit(line: &str, width: usize) -> String {
    let truncated = truncate_str(line, width, "…");
    pad_str(&truncated, width, Alignment::Left, None).into_owned()
}

/// Where `new --worktree` puts the sandbox's worktrees by default: a
/// `<sandbox>-worktrees` sibling. Mirrors `settings::worktree_dir`.
fn sibling_worktree_dir(sandbox: &Path) -> PathBuf {
    let name = sandbox
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sandbox".to_owned());
    sandbox
        .parent()
        .unwrap_or(sandbox)
        .join(format!("{name}-worktrees"))
}

fn setup_sandbox(sandbox: &Path) -> Result<()> {
    fs::create_dir_all(sandbox).context("failed to create the sandbox")?;
    run_git(sandbox, &["init", "-q", "-b", "main"])?;
    run_git(sandbox, &["config", "user.email", "guide@git-stk.dev"])?;
    run_git(sandbox, &["config", "user.name", "git-stk guide"])?;
    run_git(sandbox, &["config", "stk.provider", "demo"])?;
    run_git(sandbox, &["config", "stk.noUpdateCheck", "true"])?;
    fs::write(sandbox.join("README.md"), "# guide sandbox\n").context("failed to seed sandbox")?;
    run_git(sandbox, &["add", "README.md"])?;
    run_git(sandbox, &["commit", "-q", "-m", "initial commit"])?;
    Ok(())
}

/// Run a command in the sandbox and capture its output, forcing color on so
/// the captured lines look like a real terminal session.
fn capture(sandbox: &Path, program: impl AsRef<OsStr>, args: &[&str]) -> Result<Output> {
    let program = program.as_ref();
    isolated(Command::new(program).args(args).current_dir(sandbox))
        .env("CLICOLOR_FORCE", "1")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run {} in the sandbox", program.to_string_lossy()))
}

/// Run a `git` command in the sandbox for its effect, discarding output.
fn run_git(sandbox: &Path, args: &[&str]) -> Result<()> {
    let status = isolated(Command::new("git").args(args).current_dir(sandbox))
        .status()
        .context("failed to run git in the sandbox")?;
    if !status.success() {
        bail!("`git {}` failed in the sandbox", args.join(" "));
    }
    Ok(())
}

/// The user's global git config (e.g. stk.pushOnSubmit) must not leak into
/// the tour.
fn isolated(command: &mut Command) -> &mut Command {
    command
        .env("GIT_CONFIG_GLOBAL", nul_device())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_EDITOR", "true")
}

fn nul_device() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("NUL")
    } else {
        PathBuf::from("/dev/null")
    }
}

fn banner(title: &str) {
    anstream::println!("{}", style::paint(style::CURRENT, title));
}

fn say(line: &str) {
    anstream::println!("{}", style::paint(style::DIM, line));
}

#[cfg(test)]
mod tests {
    use super::fit;
    use console::measure_text_width;

    #[test]
    fn fit_pads_short_lines_to_exact_width() {
        let fitted = fit("ab", 5);
        assert_eq!(fitted, "ab   ");
        assert_eq!(measure_text_width(&fitted), 5);
    }

    #[test]
    fn fit_truncates_long_lines_to_exact_width() {
        let fitted = fit("abcdefghij", 4);
        assert_eq!(measure_text_width(&fitted), 4);
        assert!(fitted.ends_with('…'));
    }

    #[test]
    fn fit_measures_width_ignoring_ansi() {
        // Three visible chars wrapped in color codes, padded to width 6.
        let fitted = fit("\x1b[31mred\x1b[0m", 6);
        assert_eq!(measure_text_width(&fitted), 6);
        assert!(fitted.contains("\x1b[31m"));
    }
}
