# Pre-release smoke test

A live end-to-end pass against a real provider - the thing the test suite (which
uses fakes) can't cover. Run it once per **platform and provider**: Linux, macOS,
Windows, against a throwaway GitHub repo and a throwaway GitLab repo.

Budget ~15 min per run. A green run means `submit` → `restack` → `merge` → `sync`,
plus issue auto-close and the metadata-surgery paths (`adopt`, `repair`, `rename`),
all work against the live CLI.

## 0. Prerequisites (once per machine)

- `git` ≥ 2.38, and the provider CLI authenticated:
  - GitHub: `gh auth status`
  - GitLab: `glab auth status`
- A throwaway repo on each provider with a `main` branch and at least one commit.
  Reuse it across runs; the steps below clean up after themselves.
  - I have test repos already configured for
    [GitHub](https://github.com/lararosekelley/git-stk-test-gh) and
    [GitLab](https://gitlab.com/lararosekelley/git-stk-test-gl)

## 1. Install and verify

Install the way you ship it (pick one), then confirm the binary and setup:

```sh
brew install lararosekelley/tap/git-stk      # or: cargo install git-stk
                                             # or: the curl|sh one-liner
git stk --version                            # prints git-stk <version>
git stk setup                                # man page + shell completions
man git-stk | head -5                        # man page resolves
```

Open a new shell and confirm completion works: `git stk <TAB>`. Also test
that flags complete too with `git stk merge -<TAB>`.

## 2. Build a stack

```sh
git clone <your-test-repo-url> stk-smoke && cd stk-smoke

# Unique suffix so reruns don't collide with old remote branches.
S=$(git rev-parse --short HEAD)-$$

git stk new feat/a-$S
printf 'a\n' > a-$S.txt && git add . && git commit -m "a-$S"

git stk new feat/b-$S
printf 'b\n' > b-$S.txt && git add . && git commit -m "b-$S"

git stk list           # two branches above main, b on a
git stk provider       # detects github/gitlab from the remote
```

## 3. Submit, then inspect

```sh
git stk submit --stack --push     # creates a PR/MR per branch, b targeting a
git stk status                    # local + remote state, review numbers
git stk view                      # opens the top review in a browser
```

Check on the provider: two reviews exist, **b's base is a** (not `main`).

## 4. Restack after an amend

```sh
git stk bottom
printf 'a2\n' >> a-$S.txt && git commit -am "a-$S edit"
git stk restack --push            # b rebases onto the new a, both force-pushed
git stk run -- git log --oneline -1   # per-branch command runs bottom-up
```

Confirm the reviews updated and b still targets a.

## 5. Merge the whole stack

```sh
git stk merge --all --wait -y     # waits on checks, merges bottom-up, syncs
git stk list                      # empty: everything landed
git branch                        # feat/* branches gone, on main
```

If a check stalls, `ctrl-c` is safe and rerunning resumes.
(Optional: merge one PR manually on the web mid-wait to confirm
`git-stk` notices the out-of-band merge and syncs instead of hanging.)

## 6. Issue auto-close

`git-stk` adds a `Closes #N` line to a review when the branch name references
issue N (`123-fix`, `fix/issue-123`), so merging the review closes the issue on
the provider. This exercises issue creation, body editing, and the close-on-merge.

```sh
# Create an issue; note the number it prints (call it N):
gh   issue create --title "smoke $S" --body "tracking"                # GitHub
glab issue create --title "smoke $S" --description "tracking" --yes   # GitLab

git stk new $N-fix-$S
printf x > x-$S.txt && git add . && git commit -m "fix #$N"
git stk submit --push
```

Confirm the review body now shows `Closes #$N` (on the web, or
`gh pr view $N-fix-$S --json body` / `glab mr view $N-fix-$S`). Then:

```sh
git stk merge -y      # merge the single review
```

Confirm issue N is now **closed**: `gh issue view $N` / `glab issue view $N`.

## 7. Metadata surgery: adopt, repair, rename

The nastier local-metadata paths. Run on a fresh mini-stack:

```sh
# adopt: attach a branch created outside git-stk
git switch -c loose-$S main
printf y > y-$S.txt && git add . && git commit -m "loose work"
git stk adopt                              # attaches the current branch onto the trunk
git stk list                               # loose-$S now under main
git stk submit --push                      # give it a review (repair reads the review base)

# repair: rebuild metadata after it's lost
git config --unset branch.loose-$S.stkParent
git stk list                               # loose-$S no longer attached
git stk repair                             # restores the parent from the review base + ancestry
git stk list                               # attached again

# rename: renames the branch and retargets children locally; the review
# reconciliation is deferred to the next submit (rename only warns)
git stk new child-$S
printf z > z-$S.txt && git add . && git commit -m "child work"
git stk submit --stack --push
git stk rename loose-$S relabeled-$S       # renames + retargets child-$S, then warns that the
                                           # next submit reconciles the review
git stk list                               # main -> relabeled-$S -> child-$S; relabeled-$S has
                                           # no review number yet (!7 still heads the old name)

# the reconciling submit: closes the stale review on the old name (and deletes
# that remote branch), opens a fresh review for relabeled-$S, retargets child-$S
git stk submit --stack --push
git stk list                               # relabeled-$S now shows its own review number
```

Optional, conflict mid-`restack`: make a parent and child edit the same line, then
`git stk restack`. Resolve and `git stk continue`, or `git stk abort` to bail out
cleanly — confirm either path leaves a consistent stack.

## 8. Sanity: undo and uninstall

```sh
git stk new feat/c-$S && git stk undo   # undo restores the prior state
git stk uninstall                       # removes completions, man page, config
```

`uninstall` reports the binary path instead of deleting it; remove it by hand
if you installed manually.

Then clean up: delete `stk-smoke/`, any leftover remote branches, and close any
open reviews/issues left by steps 6–7.

## Platform notes

- **macOS / Linux:** as written above.
- **Windows — PowerShell (the full run):** the default shell, the target of the
  `.ps1` installer, and the only place the PowerShell completion wiring runs
  (`setup` falls back to PowerShell when `$SHELL` is unset). Run the whole pass.
  Watch specifically for:
  - `git stk setup` writing the PowerShell completion line to the profile, and
    `git stk <TAB>` working in a fresh PowerShell session.
  - Path separators in any printed paths (no `/`-vs-`\` mix-ups).
  - **Known gap (#211):** if `git-stk` is killed mid-operation, the lock file
    isn't auto-reclaimed on Windows. The next command should tell you exactly
    which file to delete. If it doesn't, that's a bug to file.
- **Windows — Git Bash (a quick add-on, not a full repeat):** the core loop is
  shell-independent (`git-stk` shells out to `gh`/`glab` directly, not through a
  shell), and `$SHELL=bash` makes `setup` use the same bash path as Linux/macOS.
  Confirm that:
  - `git stk setup` detects bash and wires the bash completion line.
  - The `curl | sh` one-liner installs (PowerShell users get the `.ps1`).
  - `man git-stk` resolves (Windows `man` only exists under Git Bash).
