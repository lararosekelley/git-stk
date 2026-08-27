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

`--wrapper` additionally defines the [`stk` shell function](#worktrees) whose `up`/`down`/`top`/`bottom`
`cd` into the worktree holding the branch, and teaches your shell to complete `stk` the same way it
completes `git-stk`. It is opt-in because it defines a new command name: setup skips it, telling you why,
if an `stk` executable is on your `PATH` or your rc file already defines one. bash and zsh only - fish
needs different syntax. Re-running with `--wrapper` after a plain `setup` merges it into the existing
block rather than appending a second one, and `git stk uninstall` removes the whole block either way.

## Worktrees

git-stk understands linked worktrees. `list` and `status` name the worktree holding each branch, and the
commands that rewrite history refuse up front - before touching anything - rather than failing partway
through, because git will not rebase, delete, or check out a branch another worktree has checked out.

That last one makes navigation awkward in a worktree-per-branch layout: moving up the stack is a `cd`, not
a checkout, and a program cannot change its parent shell's directory. So the navigation commands take
`--from-path`, which prints where to go and lets the shell do the moving:

`git stk setup --wrapper` writes this for you (see [Shell Completions](#shell-completions)); by hand it is:

```sh
# bash/zsh: add to ~/.bashrc or ~/.zshrc
stk() {
  case "$1" in
    up|down|top|bottom)
      local dest
      dest=$(git stk "$@" --from-path) || return
      [ -n "$dest" ] && cd "$dest"
      ;;
    *) git stk "$@" ;;
  esac
}
```

`stk up` then follows the branch wherever it lives - into another worktree when that is where it is
checked out, or an ordinary checkout when it is here (printing `.`, so your current directory is left
alone). The switch is still reported, on stderr, so stdout stays a single usable path. Every other
command falls through to `git stk` untouched, so `stk` works as the only name you need.

Capture the path before the `cd` rather than `cd "$(...)"` directly: a navigation that fails prints
nothing on stdout, and `cd ""` would add its own `cd: null directory` complaint on top of the error
git-stk already gave you.

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

```sh
# build the stack
git stk new <branch>         # stack a new branch on the current one
git stk adopt [branch]       # take an existing branch into the stack
git stk detach [branch]      # drop a branch out of the stack, keeping the branch
git stk rename [branch] <to> # rename, retargeting children and the open review
git stk split                # split one branch's commits into a stack
git stk absorb               # fold staged fixes into the commits they belong to

# move around
git stk up [branch | n]      # towards the top of the stack
git stk down [n]             # towards the trunk
git stk top                  # jump to the leaf
git stk bottom               # jump to the branch just above the trunk
git stk parent [branch]      # print the stack parent
git stk children [branch]    # print the stack children
git stk list                 # draw the stack, trunk at the bottom
git stk status [branch]      # one branch: parent, children, review, hints

# keep it stacked
git stk restack              # rebase every descendant back onto its parent
git stk run -- <command>     # run a command on each branch, bottom-up
git stk continue             # resume a restack after resolving conflicts
git stk abort                # unwind a conflicted restack
git stk undo                 # reverse the last stack-rewriting command
git stk repair               # rebuild stack metadata from reviews and ancestry

# review and land
git stk submit [--stack]     # open or update a review per branch, parent-first
git stk review [branch]      # print a branch's review
git stk view [branch]        # open it in your browser
git stk sync                 # refresh metadata, clean up landed branches, restack
git stk merge [--all]        # land the bottom review (or the whole stack), then sync
git stk cleanup [branch]     # delete landed branches and their worktrees on demand

# setup and housekeeping
git stk config               # everything stk reads or wrote
git stk provider             # which provider is detected, and why
git stk setup [--wrapper]    # man page, completions, optional `stk` shell function
git stk completions <shell>  # print the completion script for one shell
git stk guide [topic]        # interactive tours in a disposable sandbox
git stk upgrade              # move to the latest release
git stk downgrade            # step back to an earlier one
git stk uninstall            # reverse setup and the installer
git stk credits              # the stacked-workflow tools that inspired this one
```

Every flag, and the behavior behind each command, lives in
**[docs/COMMANDS.md](https://github.com/lararosekelley/git-stk/blob/main/docs/COMMANDS.md)**.

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
    ; Fetch the trunk from the remote before restacking, so branches rebase
    ; onto its latest tip. Default: false.
    fetchBeforeRestack = true
    ; Force-push (with lease) rebased branches after restack (also the restack
    ; step inside sync and merge). Default: false.
    pushOnRestack = true
    ; Push branches (-u --force-with-lease) before submitting reviews. Default: false.
    pushOnSubmit = true
    ; Bare `submit` submits the whole stack instead of one branch. Default: false.
    submitStack = true
    ; `sync` and `cleanup` also clean up branches whose review was closed
    ; without merging, not just merged ones. Default: false.
    cleanClosed = true
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
    ; Read GitHub's native stacked pull requests: `repair` prefers the stack
    ; GitHub records to the review base and to ancestry. GitHub only, and in
    ; public preview there. Default: false.
    githubStacks = true
    ; Where `new --worktree` puts a branch's worktree. Default: a
    ; <repo>-worktrees directory beside the repo.
    worktreeDir = ~/code/myrepo-worktrees
```

The tool also manages per-branch metadata: `branch.<name>.stkParent` (the stack parent),
`branch.<name>.stkBase` (the recorded fork point), `branch.<name>.stkFloor` (marking a branch as a
stack's base - see below), and - for branches made with `new --worktree` - `branch.<name>.stkWorktree`,
recording that git-stk created that worktree and so may remove it. These are written by `new`, `adopt`,
`rename`, `sync`, `restack`, `cleanup`, and `repair`; you normally never touch them by hand.

A stack does not have to sit on the trunk. Root one on any branch - a release line, say - and git-stk
records that branch as the stack's **base** (`stkFloor`). A base is not part of the stack: it is never
submitted, pushed, rebased, merged, or deleted, so a shared branch cannot be pulled into a stack and
rewritten. The marker outranks a `stkParent` too, so a base that picks one up elsewhere - metadata from an
older git-stk, say - is still left alone. The marker is what keeps that true after the branches above it land,
at which point nothing about the shape says it is a base any more. `git stk detach <branch>` clears it.

Branches are the real state; the metadata is just annotation. If it is ever lost or stale, `git stk repair`
rebuilds it from review bases (when the provider CLI - `gh`/`glab`/`tea` - is available) and branch
ancestry, and verifies recorded fork points. Anything it cannot resolve safely is reported for a manual
`git stk adopt`. With `stk.githubStacks` on, it prefers the stack GitHub itself records to both - that
ordering was stated rather than inferred, and it is still right where a review has since been retargeted.

Working across machines? The parent map - and which branch a stack sits on - rides along on a shared ref
(`refs/stk/metadata`), published automatically whenever git-stk pushes branches
(`submit`/`restack`/`sync` with push). On another clone,
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
