use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use clap::CommandFactory;

use crate::cli::Cli;
use crate::prompt::confirm;

/// Marker comment written above the completion line so re-runs can detect it
/// (`#` is also a comment in PowerShell).
const COMPLETION_MARKER: &str = "# added by git-stk setup";

/// Closes the block, so `uninstall` can lift a multi-line one out exactly.
/// Blocks written before the wrapper existed have no end marker, which is why
/// [`strip_completion_block`] still understands the single-line shape.
const BLOCK_END_MARKER: &str = "# end git-stk setup";

/// First line of the wrapper, used to recognize it in an existing block so a
/// re-run neither duplicates it nor claims it is missing.
const WRAPPER_MARKER: &str = "# stk wrapper:";

/// The `stk` function. Identical under bash and zsh - a POSIX function body,
/// `case`, and `local` all behave the same in both.
///
/// The path is captured before the `cd` rather than `cd "$(...)"`: a navigation
/// that fails prints nothing on stdout, and `cd ""` would then add its own
/// `cd: null directory` on top of the error git-stk already reported.
const WRAPPER_BODY: &str = r#"# stk wrapper: up/down/top/bottom cd into the worktree holding the branch.
# A process cannot change its parent shell's directory, so git-stk prints the
# destination and this moves you. Every other command falls through to git stk.
stk() {
  case "$1" in
    up|down|top|bottom)
      local dest
      dest=$(git stk "$@" --from-path) || return
      [ -n "$dest" ] && cd "$dest"
      ;;
    *) git stk "$@" ;;
  esac
}"#;

/// The PowerShell completion line, guarded so a removed git-stk never breaks
/// shell startup.
const POWERSHELL_LINE: &str = "if (Get-Command git-stk -ErrorAction SilentlyContinue) { git stk completions powershell | Out-String | Invoke-Expression }";

/// Reuse the completion registration git-stk already installed under the name
/// `stk`, so the wrapper completes like the command it forwards to. Both forms
/// are guarded and silenced: if the registration is missing (completions not
/// sourced yet, an older git-stk), the wrapper still works and only completion
/// is absent - nothing should be printed on every shell start.
fn completion_alias(shell: &str) -> Option<&'static str> {
    match shell {
        "bash" => Some(
            r#"complete -p git-stk >/dev/null 2>&1 && eval "$(complete -p git-stk | sed 's/ git-stk$/ stk/')""#,
        ),
        "zsh" => Some("(( $+functions[compdef] )) && compdef stk=git-stk 2>/dev/null"),
        _ => None,
    }
}

/// The wrapper is written as a bash/zsh function; fish needs different syntax
/// and PowerShell a different name-resolution story entirely.
fn wrapper_supported(shell: &str) -> bool {
    completion_alias(shell).is_some()
}

/// The block setup appends, ending in [`BLOCK_END_MARKER`] so uninstall can
/// remove it whole however many lines it grew to.
fn rc_block(shell: &str, line: &str, wrapper: bool) -> String {
    let mut block = format!("{COMPLETION_MARKER}\n{line}\n");
    if wrapper {
        block.push_str(&format!("\n{WRAPPER_BODY}\n"));
        if let Some(alias) = completion_alias(shell) {
            block.push_str(&format!("{alias}\n"));
        }
    }
    block.push_str(&format!("{BLOCK_END_MARKER}\n"));
    block
}

/// Whether something else already answers to `stk`, in which case the wrapper
/// would shadow it. Only an rc definition and a PATH executable are visible
/// from here - a function or alias defined in another sourced file is not, so
/// this reduces the chance of a collision rather than ruling one out.
fn stk_name_taken(rc: &str) -> Option<String> {
    for line in rc.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(WRAPPER_MARKER) || trimmed.starts_with("stk()") {
            continue;
        }
        if trimmed.starts_with("alias stk=") || trimmed.starts_with("function stk") {
            return Some(format!("your rc file already defines stk (`{trimmed}`)"));
        }
    }

    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join("stk");
        if candidate.is_file() {
            return Some(format!(
                "an stk executable already exists at {}",
                candidate.display()
            ));
        }
    }
    None
}

pub fn setup(yes: bool, refresh: bool, wrapper: bool) -> Result<()> {
    if refresh {
        // Re-render assets that can go stale across versions. Non-interactive;
        // run by `upgrade` via the newly installed binary. Completion wiring is
        // left alone because the rc line re-sources from the binary on every
        // shell start; missing wiring gets a hint instead of a prompt.
        install_man_page()?;
        return print_completion_hint();
    }

    install_man_page()?;
    wire_completions(yes, wrapper)?;
    Ok(())
}

/// Render the man page into the XDG data directory, which is on the default
/// manpath. This makes `git stk --help` work: git resolves it as `man git-stk`.
fn install_man_page() -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }

    let dir = man_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

    let mut buffer = Vec::new();
    clap_mangen::Man::new(Cli::command())
        .render(&mut buffer)
        .context("failed to render man page")?;

    let path = dir.join("git-stk.1");
    fs::write(&path, buffer).with_context(|| format!("failed to write {}", path.display()))?;
    anstream::println!("installed man page to {}", path.display());
    Ok(())
}

fn man_dir() -> Result<PathBuf> {
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("share"))
        })
        // Windows has no HOME; %LOCALAPPDATA% is the app-state home there.
        .or_else(|| env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .context("cannot locate a data directory; set HOME, XDG_DATA_HOME, or LOCALAPPDATA")?;
    Ok(data_home.join("man").join("man1"))
}

/// Append a completion-sourcing line to the detected shell's rc file, once,
/// plus the `stk` wrapper when asked for it.
fn wire_completions(yes: bool, wrapper: bool) -> Result<()> {
    let Some((shell, rc_path, line)) = completion_target()? else {
        anstream::println!("could not detect a supported shell");
        anstream::println!("see the README for manual completion setup");
        return Ok(());
    };

    if wrapper && !wrapper_supported(shell) {
        anstream::println!(
            "the stk wrapper is a bash/zsh shell function; {shell} needs different \
             syntax, so it was not added"
        );
        anstream::println!("see the Worktrees section of the README for a starting point");
    }
    let mut wrapper = wrapper && wrapper_supported(shell);

    let existing = match fs::read_to_string(&rc_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", rc_path.display()));
        }
    };

    // Defining `stk` on top of something else that answers to that name would
    // break whatever was there. Drop the wrapper rather than win the collision.
    if wrapper && let Some(clash) = stk_name_taken(&existing) {
        anstream::println!("skipped the stk wrapper: {clash}");
        wrapper = false;
    }

    let configured =
        existing.contains(COMPLETION_MARKER) || existing.contains("git stk completions");
    let has_wrapper = existing.contains(WRAPPER_MARKER);
    if configured && (!wrapper || has_wrapper) {
        anstream::println!(
            "{shell} completions already configured in {}",
            rc_path.display()
        );
        if wrapper_supported(shell) && !has_wrapper {
            anstream::println!(
                "{}",
                crate::style::dim(
                    "the stk wrapper (up/down cd into another worktree) is not installed; \
                     add it with `git stk setup --wrapper`"
                )
            );
        }
        return Ok(());
    }

    // Adding the wrapper to a block that is already there means replacing that
    // block, which is only safe for one we wrote: without our marker the line is
    // the user's, and rewriting would duplicate or clobber it.
    if configured && !existing.contains(COMPLETION_MARKER) {
        anstream::println!(
            "completion setup in {} was added by hand, so the wrapper was not \
             merged into it",
            rc_path.display()
        );
        anstream::println!("add this yourself:");
        for wrapper_line in rc_block(shell, line, true).lines().skip(2) {
            anstream::println!("  {wrapper_line}");
        }
        return Ok(());
    }

    // On Windows the default execution policy (Restricted, or AllSigned) blocks
    // PowerShell from loading *any* $PROFILE, so writing one would only surface
    // a "not digitally signed" error on every shell start. Guide the user to
    // relax the policy (per-user, no admin) first, rather than leaving behind a
    // profile that can't run.
    if shell == "PowerShell"
        && let Some(policy) = powershell_execution_policy()
        && policy_blocks_profile(&policy)
    {
        anstream::println!(
            "PowerShell's execution policy ({policy}) blocks profile scripts, so \
             completions can't be enabled without breaking shell startup."
        );
        anstream::println!(
            "allow your profile to run (per-user, no admin needed), then re-run `git stk setup`:"
        );
        anstream::println!("  Set-ExecutionPolicy -Scope CurrentUser RemoteSigned");
        anstream::println!("or add this line to {} yourself:", rc_path.display());
        anstream::println!("  {line}");
        return Ok(());
    }

    // Only prompt at a real terminal. Piped in (e.g. `curl ... | bash` running
    // the installer), there is no one to answer, so prompting would just print
    // a question and immediately read EOF as "no" - skip cleanly instead. Pass
    // `--yes` (or run `git stk setup` later) to wire it up non-interactively.
    let interactive = std::io::stdin().is_terminal();
    // Replacing our own block rather than appending a new one - say so, since
    // the answer decides whether an existing block gets rewritten.
    let question = if configured {
        format!("add the stk wrapper to {}? [y/N] ", rc_path.display())
    } else if wrapper {
        format!(
            "append completion setup and the stk wrapper to {}? [y/N] ",
            rc_path.display()
        )
    } else {
        format!("append completion setup to {}? [y/N] ", rc_path.display())
    };
    let proceed = if yes {
        true
    } else if interactive {
        confirm(&question)?
    } else {
        false
    };
    if !proceed {
        anstream::println!(
            "{}",
            if interactive {
                "skipped completion setup"
            } else {
                "non-interactive shell; skipped completion setup"
            }
        );
        anstream::println!("to configure manually, add this to {}:", rc_path.display());
        for block_line in rc_block(shell, line, wrapper).lines().skip(1) {
            anstream::println!("  {block_line}");
        }
        return Ok(());
    }

    // An existing block of ours is lifted out and rewritten whole, so the
    // wrapper lands inside it and uninstall still has exactly one block to find.
    let mut updated = if configured {
        strip_completion_block(&existing).unwrap_or(existing)
    } else {
        existing
    };
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!("\n{}", rc_block(shell, line, wrapper)));
    // The rc file's directory may not exist yet (fish's ~/.config/fish, a
    // never-created PowerShell profile dir).
    if let Some(parent) = rc_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&rc_path, updated)
        .with_context(|| format!("failed to write {}", rc_path.display()))?;
    if wrapper {
        anstream::println!(
            "added {shell} completion setup and the stk wrapper to {}",
            rc_path.display()
        );
        anstream::println!(
            "{}",
            crate::style::dim("start a new shell, then `stk up` follows a branch across worktrees")
        );
    } else {
        anstream::println!("added {shell} completion setup to {}", rc_path.display());
        if wrapper_supported(shell) {
            anstream::println!(
                "{}",
                crate::style::dim(
                    "`git stk setup --wrapper` also defines an stk function whose up/down \
                     cd into another worktree"
                )
            );
        }
    }
    Ok(())
}

/// Point at `git stk setup` when the detected shell has no completion
/// wiring yet. Used after upgrades, where prompting is not an option.
fn print_completion_hint() -> Result<()> {
    let Some((shell, rc_path, line)) = completion_target()? else {
        return Ok(());
    };

    let configured = fs::read_to_string(&rc_path)
        .map(|rc| rc.contains(COMPLETION_MARKER) || rc.contains("git stk completions"))
        .unwrap_or(false);
    if configured {
        return Ok(());
    }

    anstream::println!(
        "{shell} completions are not configured; run `git stk setup`, \
         or add this to {}:",
        rc_path.display()
    );
    anstream::println!("  {line}");
    Ok(())
}

/// Resolve (shell name, rc file, completion line). A POSIX shell from $SHELL
/// wins (covers Git Bash and WSL on Windows); otherwise fall back to
/// PowerShell. The lines guard on the binary existing so a removed git-stk
/// never breaks shell startup.
fn completion_target() -> Result<Option<(&'static str, PathBuf, &'static str)>> {
    if let Some(target) = posix_shell_target() {
        return Ok(Some(target));
    }
    Ok(powershell_target())
}

/// A bash/zsh/fish target from $SHELL, or None when $SHELL is unset/unknown
/// or HOME is missing (e.g. native Windows). Never an error - we fall
/// through to PowerShell.
fn posix_shell_target() -> Option<(&'static str, PathBuf, &'static str)> {
    let shell = env::var("SHELL").unwrap_or_default();
    let shell = shell.rsplit('/').next().unwrap_or_default();
    let home = env::var_os("HOME").map(PathBuf::from)?;

    match shell {
        "bash" => Some((
            "bash",
            home.join(".bashrc"),
            "command -v git-stk >/dev/null && source <(git stk completions bash)",
        )),
        "zsh" => Some((
            "zsh",
            home.join(".zshrc"),
            "command -v git-stk >/dev/null && source <(git stk completions zsh)",
        )),
        "fish" => Some((
            "fish",
            home.join(".config/fish/config.fish"),
            "command -q git-stk; and git stk completions fish | source",
        )),
        _ => None,
    }
}

/// PowerShell's `$PROFILE` (when pwsh is on PATH). Ask the shell directly -
/// the path differs across PowerShell 7 vs 5.1 and is often OneDrive-relocated.
fn powershell_target() -> Option<(&'static str, PathBuf, &'static str)> {
    for exe in ["pwsh", "powershell"] {
        let Ok(output) = Command::new(exe)
            .args(["-NoProfile", "-Command", "$PROFILE"])
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !path.is_empty() {
            return Some(("PowerShell", PathBuf::from(path), POWERSHELL_LINE));
        }
    }
    None
}

/// PowerShell's effective execution policy (`Get-ExecutionPolicy` resolves the
/// per-scope stack to one value), or None when it can't be queried.
fn powershell_execution_policy() -> Option<String> {
    for exe in ["pwsh", "powershell"] {
        let Ok(output) = Command::new(exe)
            .args(["-NoProfile", "-Command", "Get-ExecutionPolicy"])
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let policy = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !policy.is_empty() {
            return Some(policy);
        }
    }
    None
}

/// Whether an execution policy stops an unsigned `$PROFILE` from loading.
/// `Restricted` runs no scripts at all; `AllSigned` demands a digital signature
/// the profile we write does not carry. Every other policy (`RemoteSigned`,
/// `Unrestricted`, `Bypass`) runs a local profile fine.
fn policy_blocks_profile(policy: &str) -> bool {
    policy.eq_ignore_ascii_case("Restricted") || policy.eq_ignore_ascii_case("AllSigned")
}

/// Reverse `setup` and the installer: strip the completion line we added,
/// delete the man page, and remove the config/receipt directory. The binary is
/// reported (with its removal command) rather than deleted - a running exe
/// cannot reliably unlink itself, and package-manager installs must go through
/// their manager. Per-repo `stk.*` config and branch metadata are left alone.
pub fn uninstall(dry_run: bool, yes: bool) -> Result<()> {
    // The completion line, only when we can positively identify it by our own
    // marker (a hand-added line stays - we report it instead).
    let completion = match completion_target()? {
        Some((shell, rc_path, _line)) => match fs::read_to_string(&rc_path) {
            Ok(contents) if contents.contains(COMPLETION_MARKER) => {
                Some((shell, rc_path, contents))
            }
            _ => None,
        },
        None => None,
    };
    let man_page = man_dir()
        .ok()
        .map(|dir| dir.join("git-stk.1"))
        .filter(|p| p.exists());
    let config_dir = crate::upgrade::config_dir().filter(|p| p.exists());

    anstream::println!("git stk uninstall removes what setup and the installer added:");
    let mut anything = false;
    if let Some((shell, rc_path, _)) = &completion {
        anstream::println!("  - {shell} completion line in {}", rc_path.display());
        anything = true;
    }
    if let Some(path) = &man_page {
        anstream::println!("  - man page {}", path.display());
        anything = true;
    }
    if let Some(dir) = &config_dir {
        anstream::println!("  - config and install receipt in {}", dir.display());
        anything = true;
    }
    if !anything {
        anstream::println!("  (nothing found - already removed, or installed another way)");
    }

    if dry_run {
        anstream::println!("dry run: nothing was removed");
        print_binary_note();
        return Ok(());
    }
    if anything && !yes && !confirm("remove these? [y/N] ")? {
        anstream::println!("uninstall cancelled");
        print_binary_note();
        return Ok(());
    }

    if let Some((shell, rc_path, contents)) = completion
        && let Some(stripped) = strip_completion_block(&contents)
    {
        fs::write(&rc_path, stripped)
            .with_context(|| format!("failed to update {}", rc_path.display()))?;
        anstream::println!("removed {shell} completion line from {}", rc_path.display());
    }
    if let Some(path) = man_page {
        fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
        anstream::println!("removed man page {}", path.display());
    }
    if let Some(dir) = config_dir {
        fs::remove_dir_all(&dir).with_context(|| format!("failed to remove {}", dir.display()))?;
        anstream::println!("removed {}", dir.display());
    }

    print_binary_note();
    Ok(())
}

/// Tell the user how to remove the binary itself - the one thing uninstall does
/// not do, since a running process can't reliably delete its own executable and
/// package-manager installs must be removed through their manager.
fn print_binary_note() {
    anstream::println!();
    match env::current_exe() {
        Ok(path) => {
            anstream::println!("the git-stk binary is left in place; remove it with:");
            if cfg!(windows) {
                anstream::println!("  Remove-Item \"{}\"", path.display());
            } else {
                anstream::println!("  rm {}", path.display());
            }
        }
        Err(_) => anstream::println!("remove the git-stk binary from your PATH to finish."),
    }
    anstream::println!(
        "(or `cargo uninstall git-stk` / `brew uninstall git-stk` if you installed it that way)"
    );
    anstream::println!("per-repo stk.* config and branch metadata are left untouched.");
}

/// Drop the completion block `setup` appended - the [`COMPLETION_MARKER`], the
/// completion line after it, and the single blank line setup put before it.
/// `None` when there is no marker to remove.
fn strip_completion_block(contents: &str) -> Option<String> {
    let lines: Vec<&str> = contents.lines().collect();
    let marker = lines
        .iter()
        .position(|line| line.trim() == COMPLETION_MARKER)?;

    // A block setup wrote ends in BLOCK_END_MARKER and comes out whole, however
    // many lines the wrapper added. Blocks written before that marker existed
    // are "<blank>\n<marker>\n<completion line>", so fall back to removing the
    // one line after the marker, and only when it is actually ours - every
    // completion line setup writes mentions `git stk completions`. If the user
    // hand-deleted it and left the marker, the line below is their own; keep it.
    let end = match lines
        .iter()
        .skip(marker + 1)
        .position(|line| line.trim() == BLOCK_END_MARKER)
    {
        Some(offset) => marker + offset + 2,
        None => {
            let removes_completion_line = lines
                .get(marker + 1)
                .is_some_and(|line| line.contains("git stk completions"));
            marker + 1 + usize::from(removes_completion_line)
        }
    }
    .min(lines.len());
    // Also drop the single blank line setup inserted before the marker.
    let start = marker.saturating_sub(usize::from(
        marker > 0 && lines[marker - 1].trim().is_empty(),
    ));

    let mut kept = lines[..start].to_vec();
    kept.extend_from_slice(&lines[end..]);
    let mut result = kept.join("\n");
    if !result.is_empty() && contents.ends_with('\n') {
        result.push('\n');
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_removes_the_marked_block_setup_wrote() {
        // What `setup` produces: existing content, a blank line, the marker,
        // the completion line.
        let rc = "export PATH=/x\n\n# added by git-stk setup\ncommand -v git-stk >/dev/null && source <(git stk completions bash)\n";
        assert_eq!(strip_completion_block(rc).unwrap(), "export PATH=/x\n");
    }

    #[test]
    fn strip_leaves_content_after_the_block_intact() {
        // The full block, with the user's own lines below it, all preserved.
        let rc = "# added by git-stk setup\ncommand -v git-stk >/dev/null && source <(git stk completions zsh)\nalias g=git\n";
        assert_eq!(strip_completion_block(rc).unwrap(), "alias g=git\n");
    }

    #[test]
    fn strip_keeps_a_hand_edited_line_after_an_orphaned_marker() {
        // The user deleted setup's completion line but left the marker; the
        // line below is now their own and must not be removed with the marker.
        let rc = "# added by git-stk setup\nalias g=git\n";
        assert_eq!(strip_completion_block(rc).unwrap(), "alias g=git\n");
    }

    #[test]
    fn strip_returns_none_without_the_marker() {
        assert_eq!(strip_completion_block("export PATH=/x\n"), None);
    }

    #[test]
    fn strip_removes_a_wrapper_block_whole() {
        // The multi-line shape: the wrapper's own blank lines and braces must
        // not end the block early, and the user's line below has to survive.
        let rc = format!(
            "export PATH=/x\n\n{}\nalias g=git\n",
            rc_block(
                "bash",
                "command -v git-stk >/dev/null && source <(git stk completions bash)",
                true
            )
            .trim_end()
        );
        assert_eq!(
            strip_completion_block(&rc).unwrap(),
            "export PATH=/x\nalias g=git\n"
        );
    }

    #[test]
    fn strip_removes_a_wrapperless_block_with_an_end_marker() {
        let rc = format!(
            "{}\nalias g=git\n",
            rc_block(
                "zsh",
                "command -v git-stk >/dev/null && source <(git stk completions zsh)",
                false
            )
            .trim_end()
        );
        assert_eq!(strip_completion_block(&rc).unwrap(), "alias g=git\n");
    }

    #[test]
    fn a_wrapper_block_carries_the_function_and_the_completion_alias() {
        let block = rc_block("bash", "line", true);
        assert!(block.contains("stk() {"), "{block}");
        assert!(block.contains(WRAPPER_MARKER), "{block}");
        assert!(block.contains("complete -p git-stk"), "{block}");
        assert!(block.trim_end().ends_with(BLOCK_END_MARKER), "{block}");
        // zsh completes through compdef, not bash's `complete`.
        assert!(rc_block("zsh", "line", true).contains("compdef stk=git-stk"));
    }

    #[test]
    fn the_wrapper_is_bash_and_zsh_only() {
        assert!(wrapper_supported("bash") && wrapper_supported("zsh"));
        assert!(!wrapper_supported("fish") && !wrapper_supported("PowerShell"));
    }

    #[test]
    fn an_existing_stk_definition_is_detected_but_our_own_is_not() {
        assert!(stk_name_taken("alias stk=git-stk\n").is_some());
        assert!(stk_name_taken("function stk { }\n").is_some());
        // Our own block must not read as a collision, or a re-run would refuse
        // to reinstall the wrapper it wrote itself.
        assert!(stk_name_taken(&rc_block("bash", "line", true)).is_none());
    }

    #[test]
    fn blocking_policies_stop_an_unsigned_profile() {
        // The two that reject the profile we would write, case-insensitively.
        for policy in ["Restricted", "restricted", "AllSigned", "allsigned"] {
            assert!(policy_blocks_profile(policy), "{policy} should block");
        }
    }

    #[test]
    fn permissive_policies_run_a_local_profile() {
        for policy in ["RemoteSigned", "Unrestricted", "Bypass"] {
            assert!(!policy_blocks_profile(policy), "{policy} should not block");
        }
    }
}
