# Commands

The reference for the commands that build, move, and land a stack. Setting
git-stk up - `setup`, `completions`, `guide`, `uninstall` - lives in the
[README](../README.md), along with install, the quickstart, configuration, the
worktree overview, and a one-line index of every command.

## Building and inspecting the stack

```sh
git stk new <branch> [--insert | --prepend | --worktree] [--dry-run]
git stk parent [branch]
git stk children [branch]
git stk list [--all] [--commits] [--reviews] [--local] [--format <markdown|plain>]
git stk adopt [branch] [--parent <parent>] [--dry-run]   # defaults: current branch, trunk
git stk detach [branch]
git stk unstack [--dry-run] [-y]                         # dissolve the platform's own stack
git stk rename [branch] <new-name> [--dry-run]
git stk split [--per-commit] [--dry-run]
```

`new` normally stacks a fresh branch on top of where you stand. `--insert` splices it in _above_ the
current branch instead, moving the current branch's children onto it; `--prepend` splices it in _below_,
moving the current branch onto it. The new branch is empty and shares its base's tip, so descendants stay
correctly based - commit to it, then `restack` to replay them. `--prepend` needs a clean worktree.

`--worktree` creates the branch in a worktree of its own instead of checking it out here, so your current
checkout stays where it is. The directory comes from `stk.worktreeDir` (default: a `<repo>-worktrees`
directory beside the repo), with branch names nested as real directories - `feat/a` lands at `feat/a`, whose
basename still matches the branch. git-stk records the worktrees it creates and **owns removing them**:
`cleanup` takes an owned worktree away when its branch lands, but never touches one you made yourself, and
keeps any owned worktree that still has uncommitted work in it. A branch whose ref has to stay - because it
is checked out, or a worktree holds it - keeps its stack metadata too, so it stays in the stack for a later
cleanup rather than quietly dropping out of it. Running `sync` or `merge` from inside a branch's own
worktree therefore lands the review and leaves the branch alone; `cd` to the main checkout and run
`git stk cleanup <branch>` to remove the branch and its worktree. See [Worktrees](../README.md#worktrees).

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
`list --commits`. A stack's base is refused: `split` stamps a `stkParent` on the branch it splits, and a
base has none by design.

Each submitted review also carries a CI dot: `🟢` all checks passed, `🔴` one failed, `🟡` still running,
`⚪` finished without a verdict - a run that was cancelled, or one waiting on a person (GitHub's
`CANCELLED` and `ACTION_REQUIRED`, GitLab's `canceled` and `manual`). Only the newest run of each check
counts, so a run superseded by a later one on the same commit does not colour the dot; that is how the
platform decides whether a required check passed. `🕑` in place of the dot means the review is in a merge
queue.

A review the platform holds in a stack of its own is marked with its position in that stack - `⛁2/3` in
the `list` tree, and a `stack:` line in `status`. That is worth seeing: for those reviews GitHub, not
git-stk, is what merges and retargets them.

Both need fields that are in public preview, and a host whose schema lacks them - GitHub Enterprise Server,
today - rejects them. `list` retries without them and carries on, so for an open review neither the marker
nor `status`'s `stack:` line appears; `git stk -v list` says why. Registering, merging, and dissolving a
stack are unaffected - git-stk drives those over REST.

`status` and `list` append `hint:` lines pointing at the next command when there is one: `restack` when a
branch is behind its parent, `submit` when a review base went stale, `sync` when a review in the stack
merged. A stack's base gets none of those - nothing rebases, submits, or lands it - so `status` names it as
the base and points at `git stk detach` instead; a base whose own review landed, and a layer sitting on
one, are told that finishing it is theirs to do rather than sent to a command that would skip it.

`list --reviews` adds each review's approvals, comments, and requested changes beneath its branch, so the
tree doubles as a "what is waiting on whom" view. `list --local` goes the other way: it skips every provider
lookup and draws the tree from local metadata alone - no review numbers, CI dots, or queue markers, and no
network at all, which is what makes it usable offline or against a rate-limited host.

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

## Navigating and re-stacking

```sh
git stk up [branch | n]   # towards the top of the stack (children; picker at forks)
git stk down [n]          # towards the trunk (parent)
git stk top               # jump to the leaf of the stack
git stk bottom            # jump to the branch just above the trunk
git stk restack [--fetch | --no-fetch] [--update-refs | --no-update-refs] [--push | --no-push] [--dry-run]
git stk run [--fail-fast] [--no-worktree] -- <command>   # run a command on each branch, bottom-up
git stk absorb [--dry-run] [--include-unstaged]
git stk continue
git stk abort
git stk undo
```

`up` and `down` also take a distance: `git stk up 3` climbs three branches, prompting at any fork on the
way, and `git stk down 2` drops two. A distance that would walk past the end of the stack is an error
naming how far you could actually go, rather than a silent stop short of it.

`run` executes the command against each branch bottom-up (e.g. `git stk run -- cargo test`), printing a
per-branch pass/fail summary and exiting non-zero if any branch failed - a quick way to confirm each PR is
independently green before submitting. `--fail-fast` stops at the first failure.

By default it does this in a throwaway worktree under `.git`, so your own checkout never moves: uncommitted
work is fine, HEAD stays put, and no file watcher rebuilds under you. **The tradeoff is that a fresh
worktree has no untracked build output** - no `node_modules`, no `target/` - so a command that needs those
may fail there having passed in your checkout. `--no-worktree` walks your own checkout instead; that needs
a clean tree and returns you to the branch you started on. Either way the command runs in the directory you
started in, mirrored inside the scratch worktree.

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

### More on re-stacking

`restack` follows the `stk.updateRefs` config (default false). Use `--update-refs` or `--no-update-refs` to
override that for one run. If a rebase conflicts, `git-stk` records state in `.git/stack-state`; resolve
conflicts and run `git stk continue`, or run `git stk abort`.

`restack` rebases onto the local trunk, so a branch can read as "up to date" while the trunk itself is
behind its remote. When a base the stack sits on has moved on the remote, `restack` says so and points you
at `--fetch`. Pass `--fetch` (or set `git config stk.fetchBeforeRestack true`) to fast-forward the trunk from
the remote first, so branches rebase onto its latest tip; `--no-fetch` overrides the config for one run.
(`sync` always fetches the trunk, so its restack step never needs this.)

`git-stk` records each branch's fork point in `.gitconfig` as `branch.<name>.stkBase` and rebases with
`--onto`, so only a branch's own commits are replayed. This makes restacking safe after a parent is
squash-merged, rebase-merged, or amended. A missing or stale fork point falls back to a plain rebase.

After a restack, every rebased branch's remote counterpart is stale. Pass `--push` (or set
`git config stk.pushOnRestack true`) to force-push (with lease) all rebased branches automatically,
including after a conflicted restack finishes via `git stk continue`. Without it, `restack` prints the
exact push command instead. `--no-push` overrides the config for one run; `stk.remote` picks the remote
(default `origin`).

A branch whose review is sitting in a **merge queue** (GitHub) or **merge train** (GitLab) is _frozen_:
`restack` and `sync` skip both its rebase and its push, printing `frozen <branch>` instead. The queue is
merging exactly the commits already on the remote, so force-pushing would be rejected outright (GitHub
locks the branch) or silently drop the review from the queue (GitLab). Branches above a frozen one stay
put too, since their parent has not moved. Once the queued review lands, the next `sync` cleans it up and
restacks the rest as usual. If a branch is enqueued mid-run, the rejected push is reported as held rather
than failing the whole command.

## Reviews and landing

```sh
git stk provider
git stk config
git stk status [branch]
git stk review [branch]
git stk view [branch]
git stk sync [--dry-run] [--push | --no-push]
git stk merge [-y] [--auto | --all [--wait | --no-wait]] [--dry-run]
git stk repair [--dry-run | --from-remote]
git stk submit [branch] [--no-stack] [-t <title>] [-d <desc> | --desc-file <path>] [--reviewers <csv>] [--draft | --no-draft] [--ready] [--dry-run] [--push | --no-push]
git stk submit [--stack | --no-stack | --downstack] [-t <title>] [-d <desc> | --desc-file <path>] [--reviewers <csv>] [--draft | --no-draft] [--ready] [--rebuild-overview] [--dry-run] [--push | --no-push]
git stk cleanup [branch] [--dry-run] [--keep-branch]
```

`review` prints a branch's review (id, base, state, url); `view` opens it in your browser. Both work on
merged and closed reviews, and report clearly when none exists yet.

`sync` is the merge-loop one-shot: it fetches the trunk (without leaving your branch), refreshes stack
metadata from open reviews, cleans up landed branches (retargeting children and deleting), moves you off
any branch it deletes, restacks and pushes the remainder, and ends by printing the next PR to merge -
or `stack complete` when the loop is done. After squash-merging a PR, `git stk sync` is the only command
you need.

`cleanup` does the branch-deletion half on demand: it removes the local branches whose reviews have
merged (retargeting any children first). It deletes without a prompt - unlike `merge` - because a
_merged_ branch's work is already in the trunk and its ref is still in the reflog (and `git stk undo`
recreates it); `--dry-run` previews and `--keep-branch` retains them.

With `git config stk.cleanClosed true`, `sync` and `cleanup` also clean up branches whose review was
_closed_ without merging - handy if closing a PR means you are done with the branch. Those commits are
in no other branch, so git-stk treats them accordingly: the deletion line says `(closed, not merged)`
and points at `git stk undo`, and children keep the closed branch's commits rather than having them
dropped by the next restack (a merged parent's are dropped, because the trunk already has them).

`merge` merges the review at the bottom of the stack via the provider CLI (strategy from
`stk.mergeStrategy`; squash by default), confirming first unless `-y` is passed, then runs the full `sync`
flow. `merge --all` repeats that bottom-up until the stack is complete, with one confirmation up front.

- `--auto` schedules the merge when required checks are still running (GitHub `--auto`, GitLab
  auto-merge). A merge that only got scheduled - the default on GitLab - skips the sync and stops `--all`,
  telling you to rerun `git stk sync` once checks pass. Gitea has no scheduled merge, so `--auto` there
  attempts an immediate merge and a failing check surfaces as a merge error. A review in a GitHub stack
  refuses `--auto` outright: the asynchronous endpoint it must use has no scheduled mode, and merging now
  would be the opposite of what was asked. Rerun without it once checks are green.
- `--wait` (or `stk.mergeWait`) polls each review's checks until they settle before merging it, making the
  landing genuinely one command. A failing check stops the loop; `--no-wait` overrides the config. Checks
  that are queued but not yet registered are waited out, not read as "no checks."
- The wait gives up after `stk.checkTimeout` (default 30m; `0` waits forever), so a pipeline that never
  settles can't block the landing. Ctrl-c is always safe - rerun to resume from the new bottom.

`submit --stack` (and `merge --all`) operate on the stack containing the current branch - its line from
the bottom up through the current branch and out to its descendants - so it never matters where in that
stack you are standing, and independent stacks that only share the trunk are left for their own submit.
With `git config stk.submitStack true`, bare `submit` does this by default; `--no-stack` or naming a branch
submits a single branch. `restack`, `sync`, and `list` are scoped the same way - they act on the stack
containing the current branch, never the sibling stacks that merely share the trunk (which `restack`
would otherwise rebase and force-push, and `list` would otherwise draw alongside yours).

A stack need not sit on the trunk. Root one on any branch - a release line, say - and that branch is the
stack's base: the branch above it targets it, and it is never submitted, pushed, merged, or given a review
of its own, even when it has one (`merge` lands the stack's own layers, never the base's review).
`submit --stack` names the base once and submits the layers above it; since it does not push the base, and
the base is what the lowest review targets, `--push` first checks the base is on the remote and stops with
something actionable if it is not. Submitting the base itself has nothing below it to submit, and says so
rather than treating the base as unstacked - `--downstack` from it, and equally a bare `submit`,
`--no-stack`, or `submit <base>`, since `stk.submitStack` is off by default. While layers are still stacked
on it the message points at `git stk submit --stack` run _from_ that branch (`--stack` cannot be pointed at
a named one); once they have landed it simply names it as a base.

A branch with no stack parent _and_ nothing above it is not a stack at all - there is no base to target or
merge into - so `submit` and `merge` both say so. `submit` names the branch in its remedy
(`git stk adopt <branch> --parent <parent>`), because `submit <branch>` can be pointed at a branch other
than the one checked out and `adopt` defaults to the one you are on; `merge` always means the current
branch, so it leaves the name out. Either way, `git stk repair` rebuilds the metadata instead. Two things
match that description without being it: the trunk, which is not a stack branch and gets its own message
rather than an `adopt` remedy; and a _recorded base_ standing alone, which is not missing metadata - its
stack has simply landed.

Rooting a stack on a branch records it as that stack's base (`branch.<name>.stkFloor`), because the shape
alone stops showing it once the branches above it land - and shape alone cannot tell a base from a stack
whose metadata is only half rebuilt, so git-stk records intent rather than guessing. Stacking on a branch
that has no stack parent of its own is the ambiguous case: a release line and a branch nobody has adopted
yet look identical there, so `new` and `adopt` say which reading they recorded and name `git stk detach` as
the way back. `sync` then leaves the
base alone: it will not adopt a parent for it from its own review, and never counts it as finished, so a
shared release line cannot be pulled into the stack and handed to `restack` to rebase and force-push, or to
`cleanup` to delete. `git stk detach <branch>` clears the marker - as does adopting the base onto something,
which says it is a layer after all.

A recorded base outranks a recorded parent everywhere the stack is walked in order to rewrite it -
`restack`, `absorb`, `cleanup`, the metadata ref - so a base that picks up a `stkParent` elsewhere is still
never rebased, deleted, or pushed. `split` is stronger: rather than skipping a base it refuses outright,
because splitting would stamp a `stkParent` on it. That protection needs the marker, though. A stack
rooted before git-stk recorded bases does not have one: run `git stk adopt <lowest-layer> --parent <base>`
once, which records the base as it attaches the layer.

If an older git-stk already adopted that base into the stack, it carries a stack parent it should never have
had - and that makes it indistinguishable from an ordinary branch, so the adopt above records nothing. Clear
the stray parent first with `git stk detach <base>`, then run it.

With `git config stk.githubStacks true`, `submit --stack` also hands the submitted reviews to GitHub as a
[native stack](https://docs.github.com/en/pull-requests/get-started/about-stacked-prs) - bottom first, the
order it lands in - so its layers get GitHub's own stack map and parallel review. An existing stack is
extended rather than replaced. `repair` prefers that stack to the review base and to branch ancestry when
rebuilding metadata, whether or not the setting is on - it governs registering, not reading - until the
layer below has landed, at which point the platform has already retargeted the review and the listing,
which keeps the landed layer, is the staler of the two. Registering is presentation: if it fails, the
reviews still exist and the submit
still succeeds. Off by default while the GitHub feature is in public preview, and GitHub-only - GitLab and
Gitea are unaffected.

`git stk unstack` dissolves the GitHub stack for the stack you are on, leaving its reviews open and
standalone; merged reviews stay in it, since that is history GitHub keeps. It is deliberately not gated on
`stk.githubStacks` - a stack outlives the setting that created it, and may have been made outside git-stk
entirely, so undoing one must not require the setting to still be on.

It names every stack it would take apart and asks first (`-y` skips the prompt, `--dry-run` stops after the
listing). A stack is dissolved whole and several can cover one line, so this reaches reviews that are not on
your line at all - and there is no undo, since `undo` restores local metadata and this is a request to
GitHub. A dissolve that fails does not stop the rest: the remaining stacks are still attempted, and the
failures are reported at the end.

The setting gates _registering_, not _noticing_. A stack can exist without git-stk creating it - a teammate's
`gh stack submit`, the GitHub web UI - and GitHub refuses the ordinary merge and retarget for those pull
requests just the same, so git-stk reads and handles a stack whoever made it.

A stack hands two things to GitHub - whoever registered it - and git-stk follows it there. A pull request
in a stack cannot be merged through the ordinary endpoint, so `merge` uses GitHub's asynchronous merge and waits for the
result, and refuses `--auto` there because that endpoint has no scheduled mode. Its base cannot be changed
by hand either. GitHub retargets each layer onto the stack's base as the one below it lands, so `submit`
and `cleanup` report that instead of retargeting. A stack only ever puts a layer on the one recorded below
it or on its own base, so when the parent git-stk records is neither of those, the gap will not close on
its own - and `submit`, `cleanup`, `merge`, and `status` all say which of two things closes it. If the
platform has already moved the base because the layer below landed, the local stack is what is behind:
`git stk sync`. Otherwise nothing will move it - a line re-rooted onto a release branch, a reordered one,
the stack's own bottom - and dissolving the stack on the platform is what makes an ordinary retarget work
again. The local side is unchanged: git-stk still owns `restack`, `absorb`, worktrees, and the stack
metadata.

A **merge queue** on the base branch adds one more owner. The queue decides the merge method, and GitHub
rejects `stk.mergeStrategy` alongside it rather than ignoring it, so `merge` drops the strategy and says
the queue decided the method. The layer is then enqueued rather than merged, which stops `merge --all`
the way a scheduled merge does: the queue lands it on its own schedule and `git stk sync` picks it up.

`submit --downstack` submits the stack from its bottom through the current branch only, so
work-in-progress branches above you stay local. `--draft` (or `git config stk.submitDraft true`) opens
new reviews as drafts; `--no-draft` overrides the config, and `submit --ready` flips the submitted
branches' existing drafts to ready for review.

`submit --push` (or `git config stk.pushOnSubmit true`) pushes the submitted branches with
`-u --force-with-lease` before creating or updating reviews, so new branches exist remotely and rebased
ones are updated safely. If the lease is stale because the remote moved on (usually a lower branch in
the stack merged) the push is rejected; `git-stk` reports that plainly and points you at `git stk sync`
to reconcile, rather than surfacing git's raw rejection.

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

`submit --title <text>` (or `-t`) names the review, for the current or named branch only - without it a
new review takes the branch tip's commit subject, which is also what the platforms themselves default to.
A review that does not exist yet is created under the title directly, so it is never briefly published
under the commit subject; an existing one is retitled in place. On Gitea and GitLab, where draft state
lives in the title (`WIP:`, `Draft:`), the prefix is carried forward, so retitling a draft never readies
it. An empty `--title` is refused - every review needs a title, so there is nothing to clear.

`submit --desc <text>` (or `-d`) writes a description block at the top of the review body, above the
managed sections, for the current or named branch only. It sticks across resubmits until changed;
`--desc ""` removes it. `--desc-file <path>` reads the same block from a markdown or text file instead
of an inline string (handy for agent-authored bodies); it is incompatible with `--desc`, and an empty
file clears the block just like `--desc ""`. The path may be relative (to your working directory),
absolute, or start with `~`/`~/` for your home directory - the tilde is expanded even when your shell
did not (e.g. a quoted or script-supplied argument).

When the repo carries a pull/merge request template - GitHub's `PULL_REQUEST_TEMPLATE` (in the root,
`.github/`, or `docs/`), Gitea's/Forgejo's in those same spots or a `.gitea/` directory, or GitLab's
`Default.md` under `.gitlab/merge_request_templates/` - a newly created review starts from it, matching
what opening the review on the web would give you. Without a description the template is wrapped in the
managed description block, so it reads as the opening prose with `Closes #N` and the stack overview
beneath. When you pass `--desc`/`--desc-file` on a branch, your description takes the template's place on
that branch's review - the description is what you wrote, so it replaces the boilerplate rather than
sitting beneath it. A `PULL_REQUEST_TEMPLATE/` directory of named choices has no single default, so it is
skipped. Set `git config stk.usePrTemplate false` for a lean, git-stk-only body.

`submit --reviewers <csv>` requests reviews on every submitted review from a comma-separated list of
users or teams (or repeat the flag). A leading `@` is optional and stripped, so `--reviewers @foo,@bar`
and `--reviewers foo,bar` mean the same. GitHub and Gitea team reviewers use the `org/team` form
(`@my-org/backend`); GitLab has no team reviewers, so its entries are usernames. Reviewers are added, not
replaced, so a later submit never drops anyone already requested.

## Upgrading and maintenance

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
