use std::process::ExitCode;

use clap::{CommandFactory, Parser};
use git_stk::cli::{Cli, Command};
use git_stk::commands::Run;
use git_stk::{completions, style};

fn main() -> ExitCode {
    // Dynamic shell completion: when invoked by a shell's completer with
    // COMPLETE=<shell>, print candidates and exit instead of running a command.
    clap_complete::env::CompleteEnv::with_factory(Cli::command)
        .var(completions::COMPLETE_VAR)
        .complete();

    let cli = Cli::parse();
    git_stk::git::set_verbose(cli.verbose);

    // The once-a-day "newer release available" nudge, printed after the work
    // is done. Rule: the everyday workflow commands whose human-readable output
    // a person actually reads. Excluded are plumbing/scriptable output (parent,
    // children, config, completions) where a nudge would corrupt a pipe, quick
    // navigation (up/down/top/bottom) that would over-nudge, one-off structural
    // edits (new, adopt, rename, detach), rare recovery (repair, continue,
    // abort, undo), and upgrade itself.
    let hint_update = matches!(
        &cli.command,
        Command::List(_)
            | Command::Status(_)
            | Command::Sync(_)
            | Command::Submit(_)
            | Command::Merge(_)
            | Command::Restack(_)
            | Command::Cleanup(_)
    );

    // State-mutating commands take a coarse advisory lock so two git-stk runs
    // never rewrite the stack at once. Held until this scope ends; read-only
    // commands (and navigation) skip it. Failure to acquire is the error.
    let _lock = match lock_name(&cli.command) {
        Some(name) => match git_stk::lock::Lock::acquire(name) {
            Ok(lock) => Some(lock),
            Err(error) => {
                anstream::eprintln!("{} {error:#}", style::error_prefix());
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    let result = match cli.command {
        Command::New(command) => command.run(),
        Command::Absorb(command) => command.run(),
        Command::Parent(command) => command.run(),
        Command::Children(command) => command.run(),
        Command::Up(command) => command.run(),
        Command::Down(command) => command.run(),
        Command::Top(command) => command.run(),
        Command::Bottom(command) => command.run(),
        Command::List(command) => command.run(),
        Command::Status(command) => command.run(),
        Command::Adopt(command) => command.run(),
        Command::Detach(command) => command.run(),
        Command::Rename(command) => command.run(),
        Command::Restack(command) => command.run(),
        Command::Run(command) => command.run(),
        Command::Continue(command) => command.run(),
        Command::Abort(command) => command.run(),
        Command::Undo(command) => command.run(),
        Command::Provider(command) => command.run(),
        Command::Review(command) => command.run(),
        Command::View(command) => command.run(),
        Command::Sync(command) => command.run(),
        Command::Merge(command) => command.run(),
        Command::Repair(command) => command.run(),
        Command::Submit(command) => command.run(),
        Command::Config(command) => command.run(),
        Command::Completions(command) => command.run(),
        Command::Guide(command) => command.run(),
        Command::Setup(command) => command.run(),
        Command::Uninstall(command) => command.run(),
        Command::Upgrade(command) => command.run(),
        Command::Cleanup(command) => command.run(),
        Command::Credits(command) => command.run(),
    };

    match result {
        Ok(()) => {
            if hint_update {
                git_stk::upgrade::maybe_hint_update();
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            // The full anyhow context chain, on one line, behind a red
            // prefix. anstream strips the color for pipes/NO_COLOR.
            anstream::eprintln!("{} {error:#}", style::error_prefix());
            ExitCode::FAILURE
        }
    }
}

/// The lock label for a command, or `None` to run unlocked.
///
/// Rule: a command takes the lock when it rewrites the stack's commits/refs or
/// its `stkParent`/`stkBase` metadata, OR moves HEAD across the stack over a
/// window long enough that a concurrent git-stk run could clash. `None` is for
/// read-only inspection and single quick-checkout navigation, which are safe
/// alongside anything else.
fn lock_name(command: &Command) -> Option<&'static str> {
    match command {
        Command::New(_) => Some("new"),
        Command::Adopt(_) => Some("adopt"),
        Command::Detach(_) => Some("detach"),
        Command::Rename(_) => Some("rename"),
        Command::Restack(_) => Some("restack"),
        Command::Continue(_) => Some("continue"),
        Command::Abort(_) => Some("abort"),
        Command::Undo(_) => Some("undo"),
        Command::Sync(_) => Some("sync"),
        Command::Merge(_) => Some("merge"),
        Command::Repair(_) => Some("repair"),
        Command::Submit(_) => Some("submit"),
        Command::Cleanup(_) => Some("cleanup"),
        Command::Absorb(_) => Some("absorb"),
        // `run` rewrites nothing, but it checks out every branch in turn and
        // runs a user command on each - a long window where a concurrent
        // sync/restack moving those branches would derail it. The lock is held
        // for that whole window by design.
        Command::Run(_) => Some("run"),
        // Read-only, and navigation that is a single quick checkout (up/down/
        // top/bottom) - no metadata rewrite, no long window.
        Command::Parent(_)
        | Command::Children(_)
        | Command::Up(_)
        | Command::Down(_)
        | Command::Top(_)
        | Command::Bottom(_)
        | Command::List(_)
        | Command::Status(_)
        | Command::Provider(_)
        | Command::Review(_)
        | Command::View(_)
        | Command::Config(_)
        | Command::Completions(_)
        | Command::Guide(_)
        | Command::Setup(_)
        | Command::Uninstall(_)
        | Command::Upgrade(_)
        | Command::Credits(_) => None,
    }
}
