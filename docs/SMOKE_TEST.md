# Pre-release smoke test

A live end-to-end pass against a real provider - the thing the test suite (which
uses fakes) can't cover. Run it once per **platform and provider**: Linux, macOS,
Windows, against a throwaway GitHub repo and a throwaway GitLab repo.

Budget ~10 min per run. A green run means `submit` -> `restack` -> `merge` → `sync` all
work against the live CLI.

## 0. Prerequisites (once per machine)

- `git` ≥ 2.38, and the provider CLI authenticated:
  - GitHub: `gh auth status`
  - GitLab: `glab auth status`
- A throwaway repo on each provider with a `main` branch and at least one commit.
  Reuse it across runs; the steps below clean up after themselves.

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
git stk view                      # opens the bottom review in a browser
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

## 6. Sanity: undo and uninstall

```sh
git stk new feat/c-$S && git stk undo   # undo restores the prior state
git stk uninstall                       # removes completions, man page, config
```

`uninstall` reports the binary path instead of than deleting it; remove it by hand
if you installed manually.

Then delete `stk-smoke/` and any leftover remote branches.

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
