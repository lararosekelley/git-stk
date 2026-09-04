# Changelog

Notable changes per release. The section matching the version being tagged is
published into that release's GitHub notes by `dist`, so the headings must stay
`## <version>`.

## 0.12.5

### Fixed

- **merge:** a stacked review whose base branch has a merge queue still failed
  after 0.12.4. The refusal does not come back from the request: the `PUT` runs
  only basic pull request state checks, so a rule's claim on the merge method
  is a verdict from the background job and reaches git-stk at a poll, which
  0.12.4 never inspected. The refusal is now recognised wherever it arrives,
  and the merge is re-run without the method from there (#333).

## 0.12.4

### Fixed

- **merge:** a stacked review whose base branch has a merge queue no longer
  fails outright. A stacked pull request can only be merged through GitHub's
  asynchronous endpoint, which the queue governs - so it rejects the
  `stk.mergeStrategy` sent with it as an unsupported custom parameter, and does
  it as a `failed` status on a `200`, which read as a failed merge. The
  strategy is dropped and the merge re-sent when GitHub refuses it, which is
  exactly when the queue's own configured method decides the merge anyway.
  `merge` says the strategy was refused rather than switching methods silently
  (#333).

## 0.12.3

### Fixed

- **list, status:** a superseded check run no longer pins a review red. GitHub's
  rollup keeps every run recorded against the head commit, so a workflow
  cancelled by its own `concurrency` group - which `restack` triggers by pushing
  a whole stack at once - sat beside the successful re-run of the same check and
  outvoted it, for as long as that commit was the head. Only the newest run of
  each check counts now, which is how GitHub decides whether a required check
  passed (#331).
- **list, status:** a cancelled run that _is_ the newest of its check reads as
  `⚪` rather than red. Nothing failed - the run was stopped - but a required
  check in that state still blocks the merge, so neither red nor green said
  something true. `ACTION_REQUIRED`, which waits on a person, reads the same
  way, as do GitLab's `canceled` and `manual` - the dot means the same thing on
  every provider (#331).
- **merge:** `--wait` no longer merges past checks that stopped without a
  verdict - a GitLab pipeline waiting on a person, or a GitHub check whose
  newest run was cancelled. Both stop the run and say so, which is what the dot
  already reported: the gate and the dot read the same commit and disagreed.
  `gh pr checks` has no bucket for either state, so both of its settled verdicts
  are checked against the rollup rather than guessing which one they land in
  (#331).
- **list:** a commit with more than 100 check runs falls back to GitHub's own
  rollup verdict. Reading the newest run per check needs the runs, and the
  query can only ask for so many - past that a check whose only entry fell off
  would vanish, and a missing check is not a failure, so the dot could go green
  on a red commit (#331).

### Internal

- The live e2e suite runs `git stk list` against the real host, and asserts the
  stack marker on a registered stack. That marker is the only thing the batched
  annotate query produces and its per-branch fallback does not, so it is what
  tells an accepted query from a rejected one - the ids and dots look the same
  either way. The query's braces are counted in a unit test too, since it is
  assembled by hand and the fakes never send it.

## 0.12.2

### Fixed

- **docs:** `list --reviews` and `list --local` were documented nowhere; both
  are now in the command reference. `submit`'s synopsis names `--desc-file`,
  and `docs/COMMANDS.md` no longer calls itself the full reference while four
  setup commands live only in the README.
- **guide:** the `github` tour covers the base dead end - where a stack will
  not move a base and neither will git-stk - and which of `sync` or `unstack`
  closes it.

## 0.12.1

### Fixed

- **restack:** a squash-merged parent no longer makes `restack` report the
  remaining branches as missing its commits. The squash's patch id matches none
  of the originals, so comparing commits could not see the work had landed; a
  three-way merge now settles it, whatever the commit graphs look like. With
  `stk.mergeStrategy = squash` this fired after every merge in a stack (#311).
- **restack:** declining the cherry-pick offer now asks whether to discard the
  remote commits instead of failing with a command to assemble by hand. The
  push that follows uses `--force-with-lease`, which the hint on the remaining
  error now names too (#311).

## 0.12.0

### Added

- **github:** `stk.githubStacks` (off by default) lets git-stk _register_ a
  stack with GitHub's native stacked pull requests. Reading one is not gated on
  it: a stack can exist without git-stk creating it - a teammate's
  `gh stack submit`, the web UI - and GitHub refuses the ordinary merge and
  retarget for those pull requests too, so git-stk handles a stack whoever
  made it. `repair` prefers the stack GitHub records to the review base and to
  ancestry when rebuilding a branch's parent - an ordering someone stated
  rather than one inferred, which survives a wiped `.git/config` - until the
  layer below has landed, at which point the platform has already retargeted
  the review while the listing goes on naming the merged branch. With it on,
  `submit --stack`/`--downstack` also hands the submitted reviews to GitHub as
  a stack, bottom first, so the layers get GitHub's own stack map and parallel
  review; an existing stack is extended rather than replaced. Registration is
  presentation, so a failure is reported and the submit still succeeds. Off
  means git-stk creates no stack of its own; every other provider is
  untouched (#306).

  Registering a stack hands two operations to GitHub, so `merge` and `submit`
  follow it there. GitHub refuses both the synchronous merge and a manual
  retarget for a pull request in a stack, so `merge` uses the asynchronous
  merge endpoint and waits for its result - and refuses `--auto` there, since
  that endpoint has no scheduled mode - and `submit`/`cleanup` stop
  retargeting a review GitHub owns - it moves each layer itself as the one
  below it lands, and says so rather than claiming a change git-stk did not
  make. Where the stack will not bring a base to the parent git-stk records,
  `submit`, `cleanup`, `merge`, and `status` say which of two things closes the
  gap rather than promising a retarget that never comes: `git stk sync` when
  the platform has already moved the base and the local stack is behind, and
  dissolving the stack on the platform when nothing will move it - a re-rooted
  or reordered line, or the stack's own bottom. Registering itself needs
  `stk.githubStacks`; following a stack GitHub already holds does not.

- **unstack:** `git stk unstack` dissolves the platform's own stack for the
  stack you are on, leaving its reviews open and standalone - registering one
  was otherwise a one-way door, since turning `stk.githubStacks` off left every
  stack still registered. Not gated on the setting: a stack outlives it, and may
  have been made outside git-stk. It names every stack it would take apart and
  asks first - a stack is dissolved whole, so it can reach reviews outside your
  line - and `-y` skips the prompt (#315).
- **guide:** a `github` tour covering GitHub's own stacked pull requests - what
  `stk.githubStacks` hands over, what changes once a stack is registered
  (`merge` goes through GitHub's async endpoint, `--auto` is refused, git-stk
  stops retargeting), and the gotchas: `gh pr merge`/`gh pr edit --base` are
  refused by GitHub for a stacked review, detection is not gated on the
  setting, and a stack can only grow on top - so rooting a line lower means
  `unstack` and re-register (#306).
- **list, status:** show when a review sits in the platform's own stack. `list`
  marks the layer with its position (`⛁2/3`) and `status` names the stack, so it
  is visible that GitHub - not git-stk - is what merges and retargets those
  reviews. The `list` half rides the GraphQL query the annotations already use,
  so it costs no extra round trip (#316).
- **stack:** rooting a stack on a branch other than the trunk records that
  branch as the stack's base (`branch.<name>.stkFloor`). A base is never
  submitted, pushed, merged, or re-parented, and the marker is what keeps that
  true once the branches above it land - at which point nothing about the shape
  says it is a base any more. `new`, `adopt`, and the `--insert`/`--prepend`
  forms record it, naming `git stk detach <branch>` as the way back. It rides
  the shared metadata ref (`refs/stk/metadata`) alongside the parent map, so
  `repair --from-remote` rebuilds it on another machine. A stack rooted before
  this shipped has no marker and git-stk will not infer one - shape alone cannot
  tell a base from a stack that is only half rebuilt. Record it once with
  `git stk adopt <lowest-layer> --parent <base>`; if an older `sync` already
  adopted the base, run `git stk detach <base>` first (#308).

### Fixed

- **submit, merge, sync, status:** a stack rooted on a branch other than the
  trunk - a release line, say - is treated as sitting on a base rather than as a
  broken stack. Previously `submit --stack` failed the whole submit with
  `<base> has no stack parent`; `merge` offered to land the base's own review and
  `merge --all` began by landing it; `sync` adopted the base into the stack,
  giving a release line a `stkParent` that a later `restack` would rebase and
  force-push; and `status` showed the base as a branch whose metadata had gone
  missing. The base is now left out of the submit and its push (including
  `--title` and `--desc`), out of the merge plan and its count - pinned for the
  whole of `merge --all`, so the `sync` between merges cannot promote it - and
  out of `sync`'s adoption. `sync` names the lowest surviving layer as "next up"
  and reports completion into the base rather than the trunk, and `status` gives
  a base only the hints that would act on one (#307, #308).
- **submit, merge:** "no stacked branches" asks whether the repo has a stacked
  branch at all, rather than whether the trunk has children - a stack rooted off
  the trunk leaves the trunk childless, so both commands claimed a repo with one
  had none (#307).
- **submit:** a stack base the remote does not have is reported before anything
  is pushed, rather than left to the forge to reject when the review above it is
  opened against a ref that does not exist there. Submitting the base itself now
  says there is nothing below it to submit and points at `git stk submit
--stack` run from that branch, rather than reporting it as unstacked and
  offering to re-root it onto the trunk. The trunk keeps its own messages rather
  than being reported as some stack's base (#307).
- **submit, merge, status, repair, guide:** every message suggesting a bare
  `git stk adopt` now names both the branch and the parent. `adopt` defaults to
  the branch you are on _and_ to the trunk, so following the bare form while
  standing on a release line silently re-roots it - destructive when that branch
  is one a later `restack` would rebase onto the trunk and force-push (#307,
  #318).
- **split:** refuses a recorded stack base. `split` rewrites nothing, but it
  does stamp a `stkParent` on the branch it splits, and a base has none by
  design - the marker would outrank that metadata everywhere else, leaving the
  two disagreeing (#318).

## 0.11.6

### Added

- **submit:** `--title <text>` (`-t`) names the review for the current or named
  branch, instead of taking the branch tip's commit subject. A new review is
  created under the title directly; an existing one is retitled in place, and a
  draft keeps the `WIP:`/`Draft:` prefix that Gitea and GitLab encode its state
  in. Works on GitHub, GitLab, Gitea/Forgejo.

## 0.11.5

### Changed

- **docs:** the README is now an overview, with the full per-command reference
  split out into `docs/COMMANDS.md`.
- **guide:** the tours cover the newer ground - `up`/`down` taking a distance,
  `stk.cleanClosed` for reviews that are closed rather than merged, and
  `setup --wrapper` for the navigation shell function. The worktree tour also
  spells out that landing a review from inside that branch's own worktree keeps
  the branch, to be cleaned up later from the main checkout.

## 0.11.4

### Fixed

- **cleanup:** a finished branch whose ref has to stay - it is checked out, or a
  worktree holds it, or its own worktree has uncommitted work - now keeps its
  stack metadata as well, so it stays in the stack for a later cleanup instead
  of quietly dropping out of it. `cleanup` counts it as `kept` rather than
  `cleaned`; `--keep-branch` still cleans the metadata, since keeping the ref is
  the point there.
- **sync:** running `sync` (or `merge`) from inside a linked worktree no longer
  checks the trunk out there when nothing else holds it. That repointed a
  checkout made for one branch, and - the branch no longer being held - let the
  deletion take it, leaving the worktree behind with no branch and no ownership
  marker, so nothing could ever clean it up. It now stays put and keeps the
  branch, and `git stk cleanup <branch>` from the main checkout removes both.

## 0.11.3

### Added

- **cleanup:** `stk.cleanClosed` has `sync` and `cleanup` clean up branches
  whose review was closed without merging, not just merged ones.
- **nav:** `up` and `down` take an optional distance - `git stk up 3`,
  `git stk down 2` - walking that many branches and prompting at each fork the
  way `top` does. A distance past the end of the stack fails naming how far you
  could actually go, and one below 1 is rejected up front.

## 0.11.2

### Added

- **shell:** `setup --wrapper` installs an `stk` shell function whose
  `up`/`down`/`top`/`bottom` `cd` into the worktree holding the branch, and
  points your shell's completion for `git-stk` at `stk` as well. Opt-in, bash
  and zsh only: it defines a new command name, so setup skips it with a reason
  when an `stk` executable is on your `PATH` or your rc already defines one.
  Re-running with `--wrapper` merges it into the block setup already wrote, and
  `uninstall` removes the whole block.

### Fixed

- **shell:** the README's `stk` wrapper captures the destination before the
  `cd`, so a failed navigation reports only git-stk's own error instead of
  adding bash's `cd: null directory` to it.

## 0.11.1

### Fixed

- **code:** a worktree collision now suggests detaching the branch
  (`git -C <path> checkout --detach`), which works on every worktree.
  `git worktree remove` refuses on the main worktree, and re-running the
  operation from the holding worktree only helps when that worktree is the
  only one in the way - so `restack` names the main worktree as such and
  offers the delegate escape only when it would actually resolve the block.
  `undo` and the checkout collision message got the same treatment.
- **code:** the default `stk.worktreeDir` is anchored on the main worktree
  rather than the current one, so `new --worktree` run from inside a worktree
  no longer nests the next worktree under it - and `repair` looks for owned
  worktrees where they were actually put.

## 0.11.0

**git-stk now understands git worktrees.**

Commands that rewrite history used to fail partway through - or silently corrupt
the stack - when a branch was checked out in a linked worktree. Every command is
now worktree-aware, and worktree-native workflows are supported directly.

### Added

- **cli:** `new --worktree` creates a branch in a worktree of its own, under
  `stk.worktreeDir`. git-stk owns the worktrees it creates: `cleanup` removes one
  when its branch lands, never touches a worktree you made yourself, and keeps an
  owned one that still has uncommitted work. `repair` reconciles the ownership
  marker in both directions.
- **run:** runs in a throwaway worktree by default, so checking the stack no
  longer moves your HEAD or needs a clean tree. A fresh worktree has no untracked
  build output (`node_modules`, `target/`), so `--no-worktree` restores the old
  behavior for commands that need it.
- **cli:** `--from-path` on `up`/`down`/`top`/`bottom` prints where to go so a
  shell function can `cd` there - including into another worktree, which cannot
  be checked out. See the README for the snippet.
- **list:** `list` and `status` name the worktree holding each branch, and
  `list --all` spans them.
- **guide:** `git stk guide worktrees`, an interactive tour of the flows and the
  gotchas.

### Fixed

- **stack:** `undo` no longer silently rewinds a branch held by another worktree,
  which left that worktree with staged changes nobody made.
- **restack:** refuses up front instead of half-rewriting the stack, and no
  longer reports a worktree collision as a merge conflict. `continue` and `abort`
  now clear leftover state rather than wedging on it - including for repos
  already stuck.
- **code:** `cleanup` keeps a worktree-held branch and finishes the run instead
  of aborting partway.
- **sync:** `sync`, `merge`, and `restack --fetch` work from inside a worktree;
  `merge` previously failed _after_ landing the review.
- **code:** collisions explain where the branch lives instead of leaking git's
  raw `fatal:`.

### Changed

- **run:** commands now run in the directory you started in, mirrored inside the
  scratch worktree.

## 0.10.8

### Added

- **submit:** add --reviewers flag to submit, ignore template when --desc provided

## 0.10.7

### Fixed

- **submit:** refresh a stale overview row that has since merged
- **restack:** reconcile remote-only commits before force-pushing

## 0.10.6

### Added

- **submit:** support --desc-file to populate pr description

### Fixed

- **submit:** ignore generated pr description if desc/desc-file provided
- **submit:** fix handling of pr body vs commit body

### Changed

- **submit:** wrap pr template when no --desc added

## 0.10.5

### Added

- **list:** add --reviews and --local flags for rich review info

## 0.10.4

### Added

- **list:** separate stacks better in list --all

### Fixed

- **windows:** more helpful error for wrong powershell execution policy
- **run:** handle spawn errors separately from run <cmd> errors
- **stack:** avoid using stale parent in restack/repair

## 0.10.3

### Added

- **submit:** friendlier stale push error msg
- **cli:** handle merge queues
- **code:** add code review bot

### Fixed

- **code:** review bot gates on repo owner

## 0.10.2

### Fixed

- **cli:** --fetch flag for restack and config value

## 0.10.1

### Changed

- **docs:** update to include gitea language

## 0.10.0

### Added

- **cli:** finish gitea provider
- **providers:** put in place first step of gitea integration

### Fixed

- **sync:** syncing from trunk updating wrong stack notes

## 0.9.18

### Fixed

- **code:** stale lock reclamation on windows
- **cli:** refine prompting behavior

## 0.9.17

### Added

- **cli:** add downgrade command, floor of next tag

## 0.9.16

### Added

- **providers:** handle PR/MR templates better

## 0.9.15

### Fixed

- **cli:** better sibling at same stack level handling

## 0.9.14

### Added

- **providers:** append to pr template, not replace it

### Fixed

- **providers:** don't post irrelevant stack siblings on PR/MR descriptions

## 0.9.13

### Changed

- **docs:** improve help docs

## 0.9.12

### Added

- **guide:** add split guide
- **cli:** add list --commits flag
- **cli:** interactive dialogue
- **cli:** start on split cmd

## 0.9.11

Maintenance release.

## 0.9.10

Maintenance release.

## 0.9.9

### Fixed

- **providers:** gitlab bug bash

## 0.9.8

### Added

- **docs:** add issue template for bugs

### Fixed

- **merge:** make merge --wait more resilient
- **providers:** credential redaction
- **upgrade:** only remove lines that came from us in uninstall

### Changed

- **security:** supply chain trust
- **cli:** windows polish

## 0.9.7

### Fixed

- **cli:** better messaging for new users
- **stack:** close arg-injection vector in apply_remote_metadata

### Changed

- **docs:** make readme ready for launch
- **docs:** better errors when gh/glab not present or auth'd

## 0.9.6

### Fixed

- **upgrade:** backtick bug in success msg

## 0.9.5

### Added

- **cli:** add uninstall docs/command
- **cli:** stk list shows +/- changed lines

### Changed

- **code:** polish things

## 0.9.4

### Fixed

- **cli:** handle stale lock from crash
- **merge:** no more infinite hang
- **providers:** don't fail on transient errors

### Changed

- **docs:** fix some stale docs
- **docs:** better lock/hint grouping docs
- **cli:** more consistent availability of --dry-run/-n in commands
- **code:** standardize output around anstream over bare println
- **code:** stack helper consolidation
- **code:** provider detection de-duping
- **stability:** structured errors for merge issues

## 0.9.3

Maintenance release.

## 0.9.2

### Added

- **cli:** add credits command
- **cli:** list shows review numbers
- **cli:** restore stack from remote
- **docs:** add 'undo' guide tour

### Fixed

- **providers:** parse host better pt. 2
- **docs:** mention more flags in readme
- **docs:** cursor in guide always restored
- **cli:** retry on transient merge fail
- **stack:** treat trunk as empty stack line
- **stack:** share lock across worktrees
- **cli:** defer rename supersession to stack submit
- **stack:** guard walk against metadata cycles

## 0.9.1

### Added

- **cli:** forked restacking

## 0.9.0

### Added

- **cli:** add submit --rebuild-overview flag
- **docs:** add adopt/re-parenting guide

### Fixed

- **cli:** offer to close dated PRs
- **providers:** parse host better
- **cli:** correct headers passed to git apply

### Changed

- **cli:** ignore independent trunk-siblings in stk commands
- **cli:** add stack lock

## 0.8.7

### Added

- **cli:** self-hosted gitlab, linear/jira docs, nicer guide formatting on small terminals

## 0.8.6

### Added

- **cli:** add absorb guide to tutorial
- **cli:** finish absorb command
- **cli:** start on absorb command

## 0.8.5

### Added

- **cli:** insert mid-stack

### Fixed

- **cli:** make branch and --parent optional in adopt

## 0.8.4

### Added

- **cli:** list --all to list all stacks
- **cli:** git stk run command on all branches in stack

### Fixed

- **cli:** handle queued checks on github for merge --wait

## 0.8.3

### Added

- **cli:** powershell completion

### Changed

- **code:** color errors in red

## 0.8.2

Maintenance release.

## 0.8.1

### Changed

- **cli:** make merge --wait more durable

## 0.8.0

### Added

- **cli:** undo/snapshot work
- **cli:** add 'view' command to open PR/MR on web

### Changed

- **cli:** make plain-text list --format option

## 0.7.5

### Added

- **cli:** merge --wait + stk config item
- **cli:** submit --draft and --downstack

## 0.7.4

Maintenance release.

## 0.7.3

### Changed

- **cli:** proliferate styles more

## 0.7.2

### Added

- **cli:** interactive guides framework, release 0.7.2

## 0.7.1

### Added

- **cli:** show hint when new version available
- **cli:** picker when needed
- **cli:** add more color
- **cli:** give stack note editing to cleanup

### Changed

- **docs:** move to config over env vars
- **cli:** capture noisy git output

## 0.7.0

### Added

- **cli:** merge --all for full automation
- **cli:** recover deleted parent
- **cli:** handle closed without review
- **cli:** merge --auto

## 0.6.0

### Added

- **cli:** add hints on what to do next
- **cli:** remind to run stk setup post-install
- **cli:** add top/bottom commands
- **cli:** add rename command
- **cli:** flip --delete-branch to --keep-branch in stk cleanup

## 0.5.2

### Fixed

- **cli:** check for update-refs properly, update to 0.5.2

## 0.5.1

### Added

- **cli:** link issues to branch names
- **cli:** render nicer live updating PR description

### Changed

- **docs:** clean up readme
- **docs:** move TODO.md to github issues

## 0.5.0

### Added

- **cli:** merge bottom review and sync, restack idempotence
- **cli:** update sync to handle merge-restack loop in a one-shot
- **cli:** submitStack config to make bare submit cover stack
- **stack:** submit/restack from anywhere along the stack
- **docs:** add logo and crates.io badge

### Changed

- **code:** centralize stk config keys/resolution
- **cli:** move command bodies into their own modules
- **code:** per-command modules Run trait
- **providers:** split into folder

## 0.4.2

Maintenance release.

## 0.4.1

### Added

- **providers:** add list --markdown for copy-paste stack summaries

## 0.4.0

### Added

- **providers:** stack overview in more detail
- **cli:** flip stack orientation to match graphite
- **providers:** add repair cmd for metadata recovery
- **cli:** add config command/docs, move to stk.updateRefs
- **providers:** push branches before review submission
- **shell:** dynamic shell completion
- **stack:** push rebased branches after restack

### Fixed

- **cli:** version flag, annotated tag instructions, force branch delete

### Changed

- **config:** rename stack to stk in config
- **docs:** add dx notes from 5-PR stack attempt

## 0.3.0

### Added

- **providers:** include stack dependencies in PR/MR bodies
- **stack:** track fork points for squash-merge-safe restacks
- **cli:** add setup command for man page/completion installation

### Fixed

- **cli:** pass positional args to completer in git shim

### Changed

- **docs:** consolidate todos into one space

## 0.2.0

### Added

- **cli:** add shell completions subcommand

### Fixed

- **providers:** find merged reviews during cleanup

## 0.1.1

### Added

- **cli:** add stk upgrade command to pair with larakelley.com installer script

## 0.1.0

### Added

- **docs:** flesh out a readme
- **cli:** shell completion and man page generation
- **providers:** delete cleaned branches
- **providers:** add review workflows
- **stack:** add local stack workflows
- **stack:** add git command helpers
