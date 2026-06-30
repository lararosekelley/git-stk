use clap::{Parser, Subcommand};

use crate::commands;

#[derive(Debug, Parser)]
#[command(name = "git-stk")]
#[command(version)]
#[command(about = "Git-native stacked branch workflow helper, with GitHub and GitLab integration")]
#[command(after_help = "New to stacking? Run `git stk guide` for short interactive tours.")]
pub struct Cli {
    /// Pass raw git output through instead of showing it only on failure.
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,
    #[command(subcommand)]
    pub command: Command,
}

// Ordered by workflow stage - create/edit, navigate, restack/resolve,
// review/land, setup - rather than alphabetically: clap lists subcommands in
// declaration order, so `git stk --help` reads as the workflow. clap has no way
// to print headings or blank lines between subcommands, so the grouping is
// conveyed by order alone; the blank lines below are for source readability.
#[derive(Debug, Subcommand)]
pub enum Command {
    New(commands::new::New),
    Adopt(commands::adopt::Adopt),
    Split(commands::split::Split),
    Rename(commands::rename::Rename),
    Detach(commands::detach::Detach),
    Absorb(commands::absorb::Absorb),

    Up(commands::up::Up),
    Down(commands::down::Down),
    Top(commands::top::Top),
    Bottom(commands::bottom::Bottom),
    Parent(commands::parent::Parent),
    Children(commands::children::Children),
    List(commands::list::List),
    Status(commands::status::Status),

    Restack(commands::restack::Restack),
    Run(commands::run::Run),
    Continue(commands::restack::Continue),
    Abort(commands::restack::Abort),
    Undo(commands::undo::Undo),
    Repair(commands::repair::Repair),

    Submit(commands::submit::Submit),
    Review(commands::review::Review),
    View(commands::view::View),
    Sync(commands::sync::Sync),
    Merge(commands::merge::Merge),
    Provider(commands::provider::Provider),

    Config(commands::config::Config),
    Setup(commands::setup::Setup),
    Completions(commands::completions::Completions),
    Guide(commands::guide::Guide),
    Upgrade(commands::upgrade::Upgrade),
    Downgrade(commands::downgrade::Downgrade),
    Uninstall(commands::uninstall::Uninstall),
    Cleanup(commands::cleanup::Cleanup),
    Credits(commands::credits::Credits),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UpdateRefsMode {
    Config,
    Enabled,
    Disabled,
}

impl UpdateRefsMode {
    pub fn from_flags(update_refs: bool, no_update_refs: bool) -> Self {
        match (update_refs, no_update_refs) {
            (true, false) => Self::Enabled,
            (false, true) => Self::Disabled,
            _ => Self::Config,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PushMode {
    Config,
    Enabled,
    Disabled,
}

impl PushMode {
    pub fn from_flags(push: bool, no_push: bool) -> Self {
        match (push, no_push) {
            (true, false) => Self::Enabled,
            (false, true) => Self::Disabled,
            _ => Self::Config,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FetchMode {
    Config,
    Enabled,
    Disabled,
}

impl FetchMode {
    pub fn from_flags(fetch: bool, no_fetch: bool) -> Self {
        match (fetch, no_fetch) {
            (true, false) => Self::Enabled,
            (false, true) => Self::Disabled,
            _ => Self::Config,
        }
    }
}
