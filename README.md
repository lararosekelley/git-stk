<img src="https://raw.githubusercontent.com/lararosekelley/git-stk/main/assets/logo.svg"
     width="48" height="48" alt="git-stk logo" />

# git-stk

[![crates.io](https://img.shields.io/crates/v/git-stk?color=cc6699)](https://crates.io/crates/git-stk)

> Git-native stacked branch workflow helper with GitHub, GitLab, and Gitea support.

---

`git-stk` keeps stacks as ordinary Git branches. Stack parent metadata is stored locally in `.git/config` as
`branch.<name>.stkParent`, and GitHub/Gitea PR bases or GitLab MR target branches can be used to reconstruct that metadata.

![Formatted and automatically managed PR descriptions](./assets/git-stk-pr-description.png)

## Reporting issues

Planned work and known issues are tracked in
[GitHub issues](https://github.com/lararosekelley/git-stk/issues).

Feel free to report bugs, feedback, feature requests, or ask questions
there; just be polite 😉

## Install

Install using the official install script (except for PowerShell), which downloads the pre-built binary:

```sh
curl https://larakelley.com/sh/git-stk | bash
```

Or, with [Homebrew](https://brew.sh) on macOS:

```sh
brew install lararosekelley/tap/git-stk
```

With a Rust toolchain, `cargo install git-stk --locked` builds from source, or `cargo binstall git-stk`
fetches the pre-built binary without compiling.

On native Windows, use the PowerShell installer instead:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/lararosekelley/git-stk/releases/latest/download/git-stk-installer.ps1 | iex"
```

The shell command runs [`install.sh`](./install.sh) - a thin wrapper around the
[`cargo-dist`](https://github.com/axodotdev/cargo-dist)-generated `git-stk-installer.sh`; PowerShell
fetches the matching `git-stk-installer.ps1`. Every release attaches the pre-built binaries, both
installers, and per-file `.sha256` checksums to
[GitHub Releases](https://github.com/lararosekelley/git-stk/releases), so you can download and verify a
binary directly instead of piping to a shell:

```sh
# Linux x86_64, for example
sha256sum -c git-stk-x86_64-unknown-linux-musl.tar.xz.sha256
```

The Linux builds are static (musl), so they run anywhere - glibc, Alpine, or an older distro.

**Prerequisites:** review commands drive the GitHub (`gh`), GitLab (`glab`), or Gitea/Forgejo (`tea`) CLI,
so install the one you use and sign in (`gh auth login` / `glab auth login` / `tea login add`). git-stk
needs git 2.38 or newer (for `rebase --update-refs`). The local stack commands work without any CLI.

Then install the man page and wire up shell completions (idempotent; prompts before touching your shell rc):

```sh
git stk setup [-y] [--refresh]
```

Upgrade an installer-managed copy with:

```sh
git stk upgrade
```

To remove git-stk, `git stk uninstall` reverses `setup` and the installer: it strips the completion line it
added to your shell rc, deletes the man page, and removes the config/receipt directory (`--dry-run` to
preview, `-y` to skip the prompt). It prints how to remove the binary itself rather than deleting it - a
running program can't reliably delete its own executable, and a `cargo install` / Homebrew copy should go
through `cargo uninstall git-stk` or `brew uninstall git-stk`. Per-repo `stk.*` config and branch metadata
are left untouched.

## Quickstart

```sh
git stk new feature/api       # stack a branch on the current one
# ...commit work...
git stk new feature/web       # stack another on top of it
# ...commit work...
git stk submit --stack        # open a PR/MR for each branch, in order
git stk merge --all           # land them bottom-up, in one command
```

New to stacking? `git stk guide` runs the whole loop offline in a disposable sandbox - nothing real is
touched and no account is needed. The tours: `intro` (create a stack, submit, restack, land it),
`conflicts` (resolve and continue an interrupted restack), `repair` (rebuild lost stack metadata),
`absorb` (fold review fixes into the commits that introduced them), `adopt` (adopt a hand-made branch, or
move one to a new parent), and `undo` (reverse the last stack-rewriting command). `git config stk.provider
demo` turns any scratch repo into the same offline playground.

## Shell Completions

`git stk setup` configures these automatically. Completions are dynamic: the shell asks the binary for
candidates at completion time, so subcommands, flags, and even branch names complete (`git stk up <TAB>`
offers only the current branch's stack children). The installed binary prints its own registration script,
so completions stay in sync across upgrades:

```sh
# bash: add to ~/.bashrc (the guard keeps shell startup quiet if git-stk is removed)
command -v git-stk >/dev/null && source <(git stk completions bash)

# zsh: write to a directory on your fpath
git stk completions zsh > "${fpath[1]}/_git-stk"
```

```powershell
# PowerShell: add to $PROFILE (git stk setup does this for you on Windows)
if (Get-Command git-stk -ErrorAction SilentlyContinue) { git stk completions powershell | Out-String | Invoke-Expression }
```

`git stk setup` detects bash, zsh, and fish from `$SHELL` (covering Git Bash and WSL), and falls back to
PowerShell on native Windows by wiring `$PROFILE`. Elvish is also supported via `git stk completions
elvish`. The bash and zsh output includes a wrapper so git's own completion can complete `git stk <TAB>`
in addition to `git-stk <TAB>`. `-y`/`--yes` skips the shell-rc confirmation for non-interactive setup;
`--refresh` only re-renders the man page and never touches your shell rc - it is what `upgrade` runs with
the freshly installed binary.

## Install For Development

```sh
just install
just check
cargo install --path .
```

After installation, Git can use the binary as a sub-command:

```sh
git stk list
```

## Commands

Git's own narration (rebase progress, switch advice, push chatter) is captured and shown only when a
git command fails; pass `-v`/`--verbose` to any command to stream it through instead. Output is colored
when the terminal supports it; pipes and [`NO_COLOR`](https://no-color.org/) turn it off. Every command
that takes `--dry-run` also accepts `-n` as a short alias.

Local stack metadata:

```sh
git stk new <branch> [--insert | --prepend] [--dry-run]
git stk parent [branch]
git stk children [branch]
git stk list [--all] [--commits] [--format <markdown|plain>]
git stk adopt [branch] [--parent <parent>] [--dry-run]   # defaults: current branch, trunk
git stk detach [branch]
git stk rename [branch] <new-name> [--dry-run]
git stk split [--per-commit] [--dry-run]
```

`new` normally stacks a fresh branch on top of where you stand. `--insert` splices it in _above_ the
current branch instead, moving the current branch's children onto it; `--prepend` splices it in _below_,
moving the current branch onto it. The new branch is empty and shares its base's tip, so descendants stay
correctly based - commit to it, then `restack` to replay them. `--prepend` needs a clean worktree.

`rename` is `git branch -m` plus stack upkeep: children pointing at the old name are retargeted. When an
open review still heads the old branch (platforms do not follow local renames), it records the rename so
your next `submit` opens a fresh review for the new name, then offers to close the stale one and delete its
branch - leaving no orphaned PR, and the stack overview in every review drops the superseded entry.

`list` prints the stack leaf-first, like a pile sitting on its base, with the trunk labeled. Each branch
also shows, dimmed, its open review number (once submitted) and its diff size against its parent:

```text
    ◉ feature/b (#13, +42/-7)
  ○ feature/a (#12, +9/-0)
○ main (trunk)
```

The size is computed locally (no provider call), so it shows even before you submit; an empty branch and
the trunk show none.

`list --all` shows every stack at once instead of just the one you are on - the trunk once at the bottom,
each stack's tree above it, and any rootless fragments as their own trees - for an overview when several
stacks are in flight.

`list --commits` nests each branch's own commits (short SHA + subject) beneath it, newest-first, so you
can see the commit boundaries at a glance - handy before a `split`. The trunk and empty branches show none.

`split` turns the current branch's commits into a stack of branches, bottom-up, reusing the branch as the
leaf (it keeps its name and tip). It is non-destructive: the new branches point at the existing commits, so
nothing is rewritten. `--per-commit` makes one branch per commit, named from each commit's subject, with no
prompts; without it, an interactive picker lets you group commits and name each branch. Inspect first with
`list --commits`.

`status` and `list` append `hint:` lines pointing at the next command when there is one: `restack` when a
branch is behind its parent, `submit` when a review base went stale, `sync` when a review in the stack
merged.

`list --format markdown` prints a shareable summary instead - a status line and the PRs in merge order
with links and states, ready to paste into a tracking issue or PR comment:

```markdown
2 PRs, base `main`, 1 open / 1 merged

1. [Bottom change (#9)](https://github.com/owner/repo/pull/9) - merged
2. [Top change (#10)](https://github.com/owner/repo/pull/10) - open
```

For anywhere that does not render pasted markdown links (Slack, say), `--format plain` emits plain text
with bare URLs (which chat apps auto-link) instead:

```text
2 PRs, base main, 1 open / 1 merged

1. Bottom change (#9) - merged
   https://github.com/owner/repo/pull/9
2. Top change (#10) - open
   https://github.com/owner/repo/pull/10
```

Branches without reviews degrade to plain names, so both work before submitting too.

Navigation and re-stacking:

```sh
git stk up [branch]   # towards the top of the stack (children; picker at forks)
git stk down          # towards the trunk (parent)
git stk top           # jump to the leaf of the stack
git stk bottom        # jump to the branch just above the trunk
git stk restack [--update-refs | --no-update-refs] [--push | --no-push] [--dry-run]
git stk run [--fail-fast] -- <command>   # run a command on each branch, bottom-up
git stk absorb [--dry-run] [--include-unstaged]
git stk continue
git stk abort
git stk undo
```

`run` checks out each branch bottom-up and runs the command (e.g. `git stk run -- cargo test`), printing a
per-branch pass/fail summary and exiting non-zero if any branch failed - a quick way to confirm each PR is
independently green before submitting. `--fail-fast` stops at the first failure. It refuses a dirty
worktree and returns you to the branch you started on.

`absorb` takes review fixes scattered across the stack and folds each into the commit that introduced the
lines it touches: stage the fixes (`git add`), then `git stk absorb` blames each hunk, amends it into its
owning commit via a fixup + autosquash rebase, and carries every branch ref along. `--dry-run` prints the
hunk -> commit routing first; `--include-unstaged` (or `stk.absorbIncludeUnstaged`) also takes unstaged
tracked edits. Hunks it cannot attribute - new lines, trunk-owned lines, lines spanning commits - are left
in place and reported. Folding the fixes in is atomic - a conflict there rolls back untouched. Branches
that fork off below you are then restacked onto the rewritten commits; if one of those hits a conflict it
stops in the usual resumable state (`git stk continue`/`abort`, or `git stk undo` to reverse the whole
absorb).

`undo` reverses the last stack-rewriting command - `restack`, `sync`, `merge`, `cleanup`, `rename`,
`absorb`, or `new --insert`/`--prepend` - restoring local branch tips and stack metadata (it even
recreates a branch `cleanup` deleted). It is local only: pushes and platform merges are not reverted.
One level deep, it refuses on a dirty worktree (it resets the current branch) or mid-conflict (finish
with `continue`/`abort` first).

Provider-backed workflows:

```sh
git stk provider
git stk config
git stk status [branch]
git stk review [branch]
git stk view [branch]
git stk sync [--dry-run] [--push | --no-push]
git stk merge [-y] [--auto | --all [--wait | --no-wait]] [--dry-run]
git stk repair [--dry-run | --from-remote]
git stk submit [branch] [--no-stack] [-d <desc>] [--draft | --no-draft] [--ready] [--dry-run] [--push | --no-push]
git stk submit [--stack | --no-stack | --downstack] [-d <desc>] [--draft | --no-draft] [--ready] [--rebuild-overview] [--dry-run] [--push | --no-push]
git stk cleanup [branch] [--dry-run] [--keep-branch]
```

`review` prints a branch's review (id, base, state, url); `view` opens it in your browser. Both work on
merged and closed reviews, and report clearly when none exists yet.

`sync` is the merge-loop one-shot: it fetches the trunk (without leaving your branch), refreshes stack
metadata from open reviews, cleans up merged branches (retargeting children and deleting), moves you off
any branch it deletes, restacks and pushes the remainder, and ends by printing the next PR to merge -
or `stack complete` when the loop is done. After squash-merging a PR, `git stk sync` is the only command
you need.

`cleanup` does the branch-deletion half on demand: it removes the local branches whose reviews have
merged (retargeting any children first). It deletes without a prompt - unlike `merge` - because it only
touches _merged_ branches, whose work is already in the trunk and whose ref the reflog still holds (and
`git stk undo` recreates them); `--dry-run` previews and `--keep-branch` retains them.

`merge` merges the review at the bottom of the stack via the provider CLI (strategy from
`stk.mergeStrategy`; squash by default), confirming first unless `-y` is passed, then runs the full `sync`
flow. Landing a stack becomes one `git stk merge` per PR - or just `git stk merge --all`, which repeats
merge-and-sync bottom-up until the stack is complete, with a single confirmation up front. With required
checks still running, `--auto` schedules the merge instead (GitHub `--auto`, GitLab auto-merge); a merge
that only got scheduled - on GitLab that is the default - skips the sync (and stops `--all`) with a note
to rerun `git stk sync` once checks pass. Gitea has no scheduled-merge mode, so `--auto` there falls
back to an immediate merge attempt - a still-failing check surfaces as a merge error. `merge --all --wait`
(or `git config stk.mergeWait true`) polls
each review's checks until they settle before merging it - turning the landing into genuinely one command;
a failing check stops the loop, and `--no-wait` overrides the config. A freshly pushed branch whose checks
are still queued (not yet registered) is waited out, not mistaken for "no checks." The wait gives up after
`stk.checkTimeout` (default 30m; `0` waits indefinitely) so a pipeline that never settles can't block the
landing forever, and ctrl-c is always safe (rerun to resume from the new bottom).

`submit --stack` (and `merge --all`) operate on the stack containing the current branch - its line from
the bottom up through the current branch and out to its descendants - so it never matters where in that
stack you are standing, and independent stacks that only share the trunk are left for their own submit.
With `git config stk.submitStack true`, bare `submit` does this by default; `--no-stack` or naming a branch
submits a single branch. `restack`, `sync`, and `list` are scoped the same way - they act on the stack
containing the current branch, never the sibling stacks that merely share the trunk (which `restack`
would otherwise rebase and force-push, and `list` would otherwise draw alongside yours).

`submit --downstack` submits the stack from its bottom through the current branch only, so
work-in-progress branches above you stay local. `--draft` (or `git config stk.submitDraft true`) opens
new reviews as drafts; `--no-draft` overrides the config, and `submit --ready` flips the submitted
branches' existing drafts to ready for review.

`submit --push` (or `git config stk.pushOnSubmit true`) pushes the submitted branches with
`-u --force-with-lease` before creating or updating reviews, so new branches exist remotely and rebased
ones are updated safely.

`submit --stack` also maintains a stack overview at the end of every PR/MR description: the full stack as
linked bullets (leaf-first, with a pointer on the PR being viewed) sitting on the trunk, plus a footer
crediting the tool. The overview is a ledger, not a snapshot: entries are styled by status (🟢 open,
🟣 merged, 🔴 closed, the latter two struck through), and merged or closed PRs stay listed even after
their local branches are gone. `sync` (and therefore `merge`) and `cleanup` refresh the overview
mid-loop, so the remaining PRs never show stale state. The section lives between HTML comment markers and self-repairs on
the next update if the markup is hand-edited away.

Because the ledger is append-only, a row that drifted in - a PR superseded outside the rename flow, a
hand-edited body, an abandoned branch - lingers. `submit --stack --rebuild-overview` regenerates each
overview from scratch: the live stack plus genuinely merged history, dropping closed or orphaned rows.
Pair it with `--dry-run` to see which rows it would drop first. It is opt-in - the default submit keeps
preserving history.

`submit` also links issues from branch names: a branch like `123-fix-thing` or `fix/issue-123` gets a
`Closes #123` line in its PR/MR description, so the platform closes the issue when the review merges. This
auto-link is one issue per branch; to close several from one PR (or use `Fixes`, cross-repo references,
etc.), put the keywords in `--desc` (below) - the platform honors every closing keyword in the body.

Tracking work in **Linear** or **Jira** instead? You do not need anything from stk: both vendors ship a
GitHub/GitLab app that auto-links any branch or review whose name carries a ticket key
(`eng-123-fix-thing`, `PROJ-456-thing`) and recognizes magic words (`Closes ENG-123`) in review bodies.
Name your branches with the ticket key and the platform integration does the linking and closing; stk's
own `Closes #N` step stays inert on those shapes, so the two never collide.

`submit --desc <text>` (or `-d`) writes a description block at the top of the review body, above the
managed sections, for the current or named branch only. It sticks across resubmits until changed;
`--desc ""` removes it.

When the repo carries a pull/merge request template - GitHub's `PULL_REQUEST_TEMPLATE` (in the root,
`.github/`, or `docs/`), Gitea's/Forgejo's in those same spots or a `.gitea/` directory, or GitLab's
`Default.md` under `.gitlab/merge_request_templates/` - a newly created review starts from it, with the managed sections
(description, `Closes #N`, stack overview)
appended beneath rather than replacing it, matching what opening the review on the web would give you.
When those managed sections follow a template, a horizontal-rule (`---`) seam is inserted between the
two, so the automated block reads as distinct from your template rather than blurring into it.
A `PULL_REQUEST_TEMPLATE/` directory of named choices has no single default, so it is skipped. Set
`git config stk.usePrTemplate false` for a lean, git-stk-only body.

Upgrading:

```sh
git stk upgrade               # upgrade to the latest release
git stk upgrade --force       # reinstall the latest release even if up to date
git stk upgrade --head [-y]   # build and install the latest unreleased commit
git stk downgrade [-y]        # step back to the release before this one
git stk downgrade --to <ver>  # step back to a specific older release
```

`upgrade` is driven by the install receipt the shell installer writes to `~/.config/git-stk/`
(`%LOCALAPPDATA%\git-stk` on Windows): it records the installed version and where the binary lives, so
`upgrade` knows what to replace. Copies installed with `cargo install` have no receipt and should upgrade
through cargo instead.

`downgrade` is the way back when a release misbehaves: it reinstalls an earlier one through the same
receipt, so the daily check and a later `upgrade` stay correct. It is riskier than upgrading - an older
binary may not understand state a newer one wrote - so it confirms first (`-y` skips). It never steps
below the release that introduced `downgrade` itself; an older `--to` is refused rather than clamped.

`--head` requires a Rust tool-chain, prompts before installing a pre-release build, and intentionally
leaves the receipt's version stale - the HEAD build did not come from a release, so the receipt keeps
pointing at the last one. `git stk upgrade --force` is the way back onto releases afterwards.

Once a day, the everyday workflow commands (`list`, `status`, `sync`, `submit`, `merge`, `restack`,
`cleanup`) check for a newer release after their work is done - capped at five seconds, silent on any
failure or when stderr is not a terminal - and print a one-line nudge when behind. The check stamps
`update-check` next to the receipt; `git config stk.noUpdateCheck true` turns it off.

`git stk credits` lists the stacked-workflow tools that inspired git-stk, with a link to each.

## Configuration

All settings live under `[stk]` in git config, so the tool's footprint stays separated from git's own.
Everything is optional; defaults shown below:

```ini
[stk]
    ; Review provider: github, gitlab, gitea, or demo (offline playground).
    ; Default: auto-detect from the remote URL.
    provider = github
    ; Self-hosted GitLab host to detect as GitLab alongside gitlab.com (a bare
    ; host or a full URL). `glab` picks up the host from the remote itself.
    ; Default: gitlab.com only.
    gitlabHost = gitlab.example.com
    ; Self-hosted Gitea/Forgejo host to detect as Gitea alongside gitea.com and
    ; codeberg.org (a bare host or a full URL). `tea` picks up the host itself.
    ; Default: gitea.com and codeberg.org only.
    giteaHost = gitea.example.com
    ; Remote used for provider detection and pushes. Default: origin.
    remote = origin
    ; Pass --update-refs to git rebase during restack. Default: false.
    updateRefs = true
    ; Force-push (with lease) rebased branches after restack (also the restack
    ; step inside sync and merge). Default: false.
    pushOnRestack = true
    ; Push branches (-u --force-with-lease) before submitting reviews. Default: false.
    pushOnSubmit = true
    ; Bare `submit` submits the whole stack instead of one branch. Default: false.
    submitStack = true
    ; Strategy for `merge`: squash, rebase, or merge. Default: squash.
    mergeStrategy = squash
    ; `merge --all` waits for each review's checks before merging it. Default: false.
    mergeWait = true
    ; Seconds `merge --wait` polls a review's checks before giving up. 0 waits
    ; indefinitely. Default: 1800 (30m).
    checkTimeout = 1800
    ; Open new reviews as drafts. Default: false.
    submitDraft = true
    ; Seed a new review's body from the repo's PR/MR template (GitHub/Gitea
    ; PULL_REQUEST_TEMPLATE, GitLab's Default.md) instead of replacing it.
    ; Default: true.
    usePrTemplate = false
    ; `absorb` also folds unstaged tracked edits, not just staged ones. Default: false.
    absorbIncludeUnstaged = true
    ; Skip the once-a-day check for a newer release. Default: false.
    noUpdateCheck = true
```

The tool also manages per-branch metadata: `branch.<name>.stkParent` (the stack parent) and
`branch.<name>.stkBase` (the recorded fork point). These are written by `new`, `adopt`, `rename`, `sync`, `restack`,
`cleanup`, and `repair`; you normally never touch them by hand.

Branches are the real state; the metadata is just annotation. If it is ever lost or stale, `git stk repair`
rebuilds it from review bases (when the provider CLI - `gh`/`glab`/`tea` - is available) and branch
ancestry, and verifies recorded fork points. Anything it cannot resolve safely is reported for a manual `git stk adopt`.

Working across machines? The parent map rides along on a shared ref (`refs/stk/metadata`), published
automatically whenever git-stk pushes branches (`submit`/`restack`/`sync` with push). On another clone,
`git stk repair --from-remote` fetches that ref, pulls down any of its branches you do not have yet, and
rebuilds the local metadata - no platform or open PRs required. (Local-only commits you have not pushed
still can't transfer, of course.)

While a stack-rewriting command runs (`submit`, `merge`, `sync`, `restack`, `absorb`, and friends) it holds
a lock at `.git/stk-lock` so a second git-stk run cannot rewrite the stack at the same time; read-only
commands are never blocked. If a run is killed mid-operation the lock can linger, but the next
stack-rewriting command reclaims it automatically once it sees the holding process is gone (on Windows,
remove `.git/stk-lock` by hand).

Inspect everything stk reads or wrote with:

```sh
git stk config
```

## Providers

Provider detection uses `stk.provider` first, then `stk.remote`, then `origin`. GitHub support shells out
to `gh`, GitLab support shells out to `glab`, and Gitea to `tea`. Authenticate those CLIs before using provider commands.

## Re-stacking

`restack` follows the `stk.updateRefs` config (default false). Use `--update-refs` or `--no-update-refs` to
override that for one run. If a rebase conflicts, `git-stk` records state in `.git/stack-state`; resolve
conflicts and run `git stk continue`, or run `git stk abort`.

`git-stk` records each branch's fork point in `.gitconfig` as `branch.<name>.stkBase` and rebases with
`--onto`, so only a branch's own commits are replayed. This makes restacking safe after a parent is
squash-merged, rebase-merged, or amended. A missing or stale fork point falls back to a plain rebase.

After a restack, every rebased branch's remote counterpart is stale. Pass `--push` (or set
`git config stk.pushOnRestack true`) to force-push (with lease) all rebased branches automatically,
including after a conflicted restack finishes via `git stk continue`. Without it, `restack` prints the
exact push command instead. `--no-push` overrides the config for one run; `stk.remote` picks the remote
(default `origin`).

## Generated Assets

Shell completions and a `man` page can be generated with:

```sh
just generate-assets
```

Generated files are written under `target/generated`.

## Project Tasks

```sh
just build
just test
just lint
just check
```

## License

Copyright (c) 2026 [Lara Kelley](https://larakelley.com). MIT License. See [LICENSE](./LICENSE).
