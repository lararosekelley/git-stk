# Changelog

Notable changes per release. The section matching the version being tagged is
published into that release's GitHub notes by `dist`, so the headings must stay
`## <version>`.

## 0.12.0

### Added

- **github:** `stk.githubStacks` (off by default) lets git-stk read GitHub's
  native stacked pull requests. With it on, `repair` prefers the stack GitHub
  records to the review base and to ancestry when rebuilding a branch's parent
  - an ordering someone stated rather than one inferred, which survives a wiped
  `.git/config` and is still right where a review has since been retargeted.
  With it on, `submit --stack`/`--downstack` also hands the submitted reviews
  to GitHub as a stack, bottom first, so the layers get GitHub's own stack map
  and parallel review; an existing stack is extended rather than replaced.
  Registration is presentation, so a failure is reported and the submit still
  succeeds. Off means today's behaviour byte for byte, and every other provider
  is untouched (#306).

  Not yet wired to `merge`: GitHub's docs say a stack cannot be merged with the
  synchronous endpoints `merge_review` uses, and that needs a live check before
  anything changes there.
- **stack:** rooting a stack on a branch other than the trunk records that
  branch as the stack's base (`branch.<name>.stkFloor`). A base is never
  submitted, pushed, merged, or re-parented, and recording it is what makes
  that hold once the branches above it land - at which point nothing about the
  shape says it is a base any more. `new`, `adopt`, and the `--insert` /
  `--prepend` forms record it, saying so and naming `git stk detach <branch>`
  as the way back - stacking on a branch that has no stack parent of its own is
  ambiguous, since a release line and a branch nobody has adopted yet look
  identical there. A stack
  rooted before this shipped has no marker, and git-stk does not infer one -
  shape alone cannot tell a base from a stack that is only half rebuilt, and
  guessing wrong freezes an ordinary branch out of its own restacks. Record it
  once with `git stk adopt <lowest-layer> --parent <base>`, which marks the
  base as it attaches the layer. A base an older `sync` already adopted carries
  a stack parent it should never have had, which makes it indistinguishable
  from an ordinary branch: run `git stk detach <base>` first, then the adopt
  above (#308). The base rides the shared metadata ref
  (`refs/stk/metadata`) alongside the parent map, so `repair --from-remote`
  rebuilds it on another machine; `adopt` and `undo` clear it again.

### Fixed

- **submit:** a stack rooted on a branch other than the trunk - a release line,
  say - no longer fails the whole stack-mode submit with
  `<base> has no stack parent`. That root is the stack's base: the branch above
  it targets it, and it is left out of the submit, the `-u --force-with-lease`
  push, and every step that writes to a review - including `--title` and
  `--desc`, which act on the current branch and so must skip it when that is
  the base - exactly as `restack` and `absorb` already treat it. `--no-stack`
  always handled this shape, and `new`/`adopt` both
  create it, so stack mode refusing it was an accept-then-reject asymmetry
  (#307).
- **merge:** a stack rooted on a branch other than the trunk no longer treats
  that base as the stack bottom. If the base had a review of its own - a
  release branch with an open PR into the trunk, say - `merge` would offer to
  land _that_ review, and `merge --all` would start by landing it; the base
  check that guards this is skipped for a branch with no recorded parent, so
  nothing caught it. `merge --all` also counted the base among the reviews it
  set out to land (#307).
- **status:** a stack's base is named as one rather than shown as a branch
  whose metadata went missing, and its hints changed with it: no `restack` when
  it is behind the trunk (nothing rebases it), and no `sync` when its own
  review lands, or when the review of a base a branch sits on lands - `sync`
  skips a base by design, so both pointed at a command that would never act.
  Each now names what is actually left to do (#308).
- **sync:** a stack rooted on a branch other than the trunk no longer has that
  base adopted into the stack. `sync` recorded a parent for every branch in
  scope from its own review, and the base is in scope - so a release line with
  an open PR into the trunk picked up `stkParent = <trunk>`, which `restack`
  would then rebase and force-push, and a merged release PR would have deleted
  the branch locally. A branch with no parent and nothing stacked on it is
  still adopted: that is metadata to rebuild, not a base (#308).
- **sync:** the closing "next up" line names the lowest surviving layer rather
  than the stack's base, which has nothing of ours to review or land (#308).
- **sync:** a stack rooted off the trunk reports as complete into its own base
  rather than into the trunk, which is not where its layers landed (#308).
- **merge:** `merge --all` pins the stack's base for the whole run. The `sync`
  between merges re-records that base's parent from its own review (#308), so
  recomputing the bottom each iteration could promote it into the landing and
  merge a release PR the up-front confirmation never offered - it named the
  base as the destination, not as something being merged (#307).
- **submit:** a stack base the remote does not have is now reported before
  anything is pushed, rather than left to the forge to reject when the review
  above it is opened against a ref that does not exist there (#307).
- **submit:** submitting the stack's base now says there is nothing below it to
  submit and points at `git stk submit --stack` run from that branch, rather
  than reporting the base as unstacked and offering to re-root it onto the
  trunk - which is destructive when the base is a release line. Covers
  `--downstack` and, since `stk.submitStack` is off by default, a bare
  `submit`, `--no-stack`, and `submit <base>` (#307).
- **submit:** the trunk keeps its own messages on the default path, rather than
  being reported as a stack's base. Naming it (`git stk submit <trunk>`) says
  it is never part of a stack; standing on it says to check out a stacked
  branch (#307).
- **submit, merge:** "no stacked branches" now asks whether the repo has a
  stacked branch at all, rather than whether the trunk has children - a stack
  rooted off the trunk leaves the trunk childless, so both commands claimed a
  repo with one had none (#307).
- **merge:** on a branch with no stack parent, "no stacked branches to merge"
  now names the branch and points at `git stk adopt --parent <parent>` or
  `git stk repair`, rather than implying the repo has no stacks (#307).
- **submit:** the "no stack parent" error now names the branch being submitted
  in both halves - `git stk adopt <branch> --parent <parent>` - rather than a
  bare `git stk adopt`, which silently adopts onto the trunk - a destructive
  suggestion when the branch in question is a release line that a later
  `restack` would rebase onto the trunk and force-push (#307).

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
