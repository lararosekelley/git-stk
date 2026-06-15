# Pre-release manual checks

The live e2e suite ([`.github/workflows/e2e.yml`](../.github/workflows/e2e.yml))
gates every release. It runs the full stacked-branch lifecycle (`submit` →
`restack` → `merge` → `sync`), issue auto-close, `adopt`/`repair`/`rename`,
conflict `continue`/`abort`, and completions + `setup` + `uninstall` — on GitHub
and GitLab, across Linux (bash), macOS (zsh), and Windows (pwsh), each against an
ephemeral repo.

This doc is only the things CI **structurally can't** exercise. Run them by hand
before announcing a release.

## 1. Install methods

The e2e builds from source and runs the binary directly — it never touches the
published installers. Verify each lands a working binary (`git stk --version`):

```sh
brew install lararosekelley/tap/git-stk
cargo install git-stk --locked
cargo binstall git-stk
curl https://larakelley.com/sh/git-stk | bash        # macOS / Linux / Git Bash
```

```powershell
# native Windows
powershell -ExecutionPolicy Bypass -c "irm https://github.com/lararosekelley/git-stk/releases/latest/download/git-stk-installer.ps1 | iex"
```

After at least one install, confirm `man git-stk | head` renders (the e2e runs
`setup` but never renders the man page).

## 2. Upgrade and uninstall of an installer-managed copy

The e2e covers `setup` → `uninstall`, but not the installer's receipt path:

```sh
git stk upgrade      # installer-managed copy; receipt-driven
git stk uninstall    # reverses installer + setup; reports the binary path rather than deleting it
```

(A `cargo install` / Homebrew copy has no receipt — `upgrade` should point you at
`cargo`/`brew` instead.)

## 3. Interactive prompts (#217)

CI passes `-y` everywhere, so confirm flows never run live. In a real terminal,
on a scratch stack:

- `git stk merge` (no `-y`) prompts and respects `y`/`N`.
- After `git stk rename`, the next `git stk submit` prompts to close the stale
  review (default-yes). Answer it cleanly — don't type ahead (the buffered-input
  hazard in #217).

## 4. Windows stale-lock reclaim (#211)

Can't be reproduced on a clean CI runner. On Windows: kill `git-stk` mid-operation
(so the lock file is left behind), then run another command. It should tell you
exactly which lock file to delete — it isn't auto-reclaimed on Windows yet. If
the message is missing or wrong, that's a bug.

## Optional: offline guide

`git stk guide` walks the workflow in a disposable sandbox (no account, nothing
real touched) — a quick sanity check that the tours still run.
