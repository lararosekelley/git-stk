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

#[derive(Debug, Subcommand)]
pub enum Command {
    New(commands::new::New),
    Absorb(commands::absorb::Absorb),
    Parent(commands::parent::Parent),
    Children(commands::children::Children),
    Up(commands::up::Up),
    Down(commands::down::Down),
    Top(commands::top::Top),
    Bottom(commands::bottom::Bottom),
    List(commands::list::List),
    Status(commands::status::Status),
    Adopt(commands::adopt::Adopt),
    Detach(commands::detach::Detach),
    Rename(commands::rename::Rename),
    Restack(commands::restack::Restack),
    Run(commands::run::Run),
    Continue(commands::restack::Continue),
    Abort(commands::restack::Abort),
    Undo(commands::undo::Undo),
    Provider(commands::provider::Provider),
    Review(commands::review::Review),
    View(commands::view::View),
    Sync(commands::sync::Sync),
    Merge(commands::merge::Merge),
    Repair(commands::repair::Repair),
    Submit(commands::submit::Submit),
    Config(commands::config::Config),
    Completions(commands::completions::Completions),
    Guide(commands::guide::Guide),
    Setup(commands::setup::Setup),
    Uninstall(commands::uninstall::Uninstall),
    Upgrade(commands::upgrade::Upgrade),
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
