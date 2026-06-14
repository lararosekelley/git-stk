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

/// The PowerShell completion line, guarded so a removed git-stk never breaks
/// shell startup.
const POWERSHELL_LINE: &str = "if (Get-Command git-stk -ErrorAction SilentlyContinue) { git stk completions powershell | Out-String | Invoke-Expression }";

pub fn setup(yes: bool, refresh: bool) -> Result<()> {
    if refresh {
        // Re-render assets that can go stale across versions. Non-interactive;
        // run by `upgrade` via the newly installed binary. Completion wiring is
        // left alone because the rc line re-sources from the binary on every
        // shell start; missing wiring gets a hint instead of a prompt.
        install_man_page()?;
        return print_completion_hint();
    }

    install_man_page()?;
    wire_completions(yes)?;
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
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .context("cannot locate a data directory; set HOME or XDG_DATA_HOME")?;
    Ok(data_home.join("man/man1"))
}

/// Append a completion-sourcing line to the detected shell's rc file, once.
fn wire_completions(yes: bool) -> Result<()> {
    let Some((shell, rc_path, line)) = completion_target()? else {
        anstream::println!("could not detect a supported shell");
        anstream::println!("see the README for manual completion setup");
        return Ok(());
    };

    let existing = match fs::read_to_string(&rc_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", rc_path.display()));
        }
    };

    if existing.contains(COMPLETION_MARKER) || existing.contains("git stk completions") {
        anstream::println!(
            "{shell} completions already configured in {}",
            rc_path.display()
        );
        return Ok(());
    }

    // Only prompt at a real terminal. Piped in (e.g. `curl ... | bash` running
    // the installer), there is no one to answer, so prompting would just print
    // a question and immediately read EOF as "no" - skip cleanly instead. Pass
    // `--yes` (or run `git stk setup` later) to wire it up non-interactively.
    let interactive = std::io::stdin().is_terminal();
    let proceed = if yes {
        true
    } else if interactive {
        confirm(&format!(
            "append completion setup to {}? [y/N] ",
            rc_path.display()
        ))?
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
        anstream::println!("  {line}");
        return Ok(());
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!("\n{COMPLETION_MARKER}\n{line}\n"));
    // The rc file's directory may not exist yet (fish's ~/.config/fish, a
    // never-created PowerShell profile dir).
    if let Some(parent) = rc_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&rc_path, updated)
        .with_context(|| format!("failed to write {}", rc_path.display()))?;
    anstream::println!("added {shell} completion setup to {}", rc_path.display());
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
            anstream::println!("  rm {}", path.display());
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

    // The block is "<blank>\n<marker>\n<completion line>". Only remove the line
    // after the marker when it is actually ours - every completion line setup
    // writes mentions `git stk completions`. If the user hand-deleted it and
    // left the marker, the line below is their own content; keep it.
    let removes_completion_line = lines
        .get(marker + 1)
        .is_some_and(|line| line.contains("git stk completions"));
    let end = (marker + 1 + usize::from(removes_completion_line)).min(lines.len());
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
}
