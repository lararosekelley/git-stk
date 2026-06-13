use std::env;
use std::ffi::OsStr;
use std::io::{self, Write};

use anyhow::{Context, Result, bail};
use clap_complete::Shell;
use clap_complete::engine::CompletionCandidate;
use clap_complete::env::Shells;

use crate::{git, stack};

/// Environment variable that triggers dynamic completion (see `main`).
pub const COMPLETE_VAR: &str = "COMPLETE";

/// Lets git's bash completion handle `git stk <TAB>` by delegating to the
/// dynamic clap completer, which only knows the `git-stk` binary form. The
/// current word is forwarded so prefix filtering keeps working.
const BASH_GIT_SHIM: &str = r#"
_git_stk() {
    local cur="${COMP_WORDS[COMP_CWORD]}"
    COMP_WORDS=("git-stk" "${COMP_WORDS[@]:2}")
    COMP_CWORD=$((COMP_CWORD - 1))
    _clap_complete_git_stk git-stk "$cur" ""
}
"#;

/// Same bridge for zsh: its `_git` dispatcher calls `_git-stk` for
/// `git stk <TAB>`. zsh's dynamic scoping makes the rewritten `words` and
/// `CURRENT` visible to the clap completer.
const ZSH_GIT_SHIM: &str = r#"
function _git-stk() {
    local -a words=("git-stk" "${words[@]:2}")
    local CURRENT=$((CURRENT - 1))
    _clap_dynamic_completer_git_stk
}
"#;

pub fn print(shell: Shell) -> Result<()> {
    // Point the registration at this exact binary so completion works even
    // when git-stk is not on PATH (and stays correct across upgrades, since
    // shells re-source this output on every start).
    let completer = env::current_exe()
        .ok()
        .and_then(|path| path.to_str().map(str::to_owned))
        .unwrap_or_else(|| "git-stk".to_owned());

    write(shell, &completer, &mut io::stdout().lock())
}

/// Write the dynamic-completion registration script for `shell`, with shims
/// so the `git stk` subcommand form completes too. `completer` is the binary
/// the shell invokes at completion time.
pub fn write(shell: Shell, completer: &str, writer: &mut dyn Write) -> Result<()> {
    let shells = Shells::builtins();
    let name = shell.to_string();
    let Some(env_completer) = shells.completer(&name) else {
        bail!("no dynamic completion support for {name}");
    };

    env_completer
        .write_registration(COMPLETE_VAR, "git-stk", "git-stk", completer, writer)
        .with_context(|| format!("failed to write {name} completion registration"))?;

    match shell {
        Shell::Bash => write!(writer, "{BASH_GIT_SHIM}")?,
        Shell::Zsh => write!(writer, "{ZSH_GIT_SHIM}")?,
        _ => {}
    }

    Ok(())
}

/// Complete branch-name arguments with local branches.
pub fn branch_candidates(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };

    git::local_branches()
        .unwrap_or_default()
        .into_iter()
        .filter(|branch| branch.starts_with(prefix))
        .map(CompletionCandidate::new)
        .collect()
}

/// Complete `up` with the current branch's stack children only.
pub fn child_branch_candidates(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    let Ok(branch) = git::current_branch() else {
        return Vec::new();
    };

    stack::children_of(&branch)
        .unwrap_or_default()
        .into_iter()
        .filter(|child| child.starts_with(prefix))
        .map(CompletionCandidate::new)
        .collect()
}
