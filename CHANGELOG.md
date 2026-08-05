# Changelog

Notable changes per release. The section matching the version being tagged is
published into that release's GitHub notes by `dist`, so the headings must stay
`## <version>`.

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
