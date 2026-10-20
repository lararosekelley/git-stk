use std::path::PathBuf;

use std::cell::RefCell;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Mutex;
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use clap::ArgAction;
use clap_complete::engine::ArgValueCompleter;

use crate::agent::Agent;
use crate::browser;
use crate::commands::Run;
use crate::completions;
use crate::git;
use crate::progress::Progress;
use crate::prompt;
use crate::providers::{
    Anchor, DraftComment, ProviderKind, ReviewEvent, ReviewProvider, ReviewRef, ReviewRequest,
    Side, Submission, Thread, detect_review_provider,
};
use crate::settings::{self, TourStyle};
use crate::style;
use crate::tour::server::{Request, Response, Server};
use crate::tour::{self, Planner, Source, Tour as ReviewTour};

/// Open a step-by-step tour of a review's changes in the browser
/// (experimental).
#[derive(Debug, clap::Args)]
pub struct Tour {
    /// Branch, review number (12, #12), or review URL (defaults to the current
    /// branch).
    #[arg(value_name = "TARGET", add = ArgValueCompleter::new(completions::branch_candidates))]
    target: Option<String>,
    /// Tour only this review, not the stack of reviews around it.
    #[arg(long, action = ArgAction::SetTrue)]
    no_stack: bool,
    /// How to lay out the tour: `standard` groups files by role; `agentTour`
    /// has the tour agent plan its steps; `agentReview` adds the agent's
    /// review findings (overrides stk.tourStyle).
    #[arg(long, value_name = "STYLE")]
    style: Option<TourStyle>,
    /// The command an agent tour is planned with, such as `claude -p` - your
    /// own harness, which holds its own credentials (overrides stk.tourAgent).
    #[arg(long, value_name = "COMMAND")]
    agent: Option<String>,
    /// Follow this plan, written ahead of time, for the review asked for
    /// instead of asking the agent. Implies `--style agentTour`.
    #[arg(long, value_name = "FILE")]
    plan: Option<PathBuf>,
    /// Ask the agent again rather than use plans saved from earlier tours.
    #[arg(long, action = ArgAction::SetTrue)]
    refresh_plan: bool,
    /// Print the tour's address without opening a browser.
    #[arg(long, action = ArgAction::SetTrue)]
    no_open: bool,
    /// Write a read-only snapshot of the tour to this file instead of
    /// serving it.
    #[arg(long, short = 'o', value_name = "PATH", conflicts_with = "findings")]
    output: Option<PathBuf>,
    /// Print the agent review's findings for the review asked for as JSON,
    /// for your own agent to work from, instead of opening a tour. Uses the
    /// review saved from an earlier tour of the same commits when there is
    /// one.
    #[arg(long, action = ArgAction::SetTrue)]
    findings: bool,
    /// Tour a branch from local refs, as when it has no review yet, even
    /// when it has one: its own commits against its stack parent, with
    /// nothing to post.
    #[arg(long, action = ArgAction::SetTrue)]
    local: bool,
}

impl Run for Tour {
    fn run(self) -> Result<()> {
        let style = if self.findings {
            match self.style {
                None | Some(TourStyle::AgentReview) => Some(TourStyle::AgentReview),
                Some(_) => bail!("--findings prints an agent review's findings; drop --style"),
            }
        } else {
            self.style
        };
        let planner = planner(
            style,
            self.agent.as_deref(),
            self.plan.as_deref(),
            self.refresh_plan,
            !self.findings,
        )?;
        let mode = match (self.findings, self.output) {
            (true, _) => Mode::Findings,
            (false, Some(path)) => Mode::Snapshot {
                path,
                open: !self.no_open,
            },
            (false, None) => Mode::Serve {
                open: !self.no_open,
            },
        };
        tour_command(
            self.target.as_deref(),
            mode,
            !self.no_stack,
            self.local,
            planner.as_ref(),
        )
    }
}

/// What the tour command produces.
enum Mode {
    /// Serve the page, opening a browser when `open`.
    Serve { open: bool },
    /// Write a read-only snapshot to `path`.
    Snapshot { path: PathBuf, open: bool },
    /// Print the agent review of the review asked for as JSON.
    Findings,
}

fn tour_command(
    target: Option<&str>,
    mode: Mode,
    stack: bool,
    local: bool,
    planner: Option<&Planner>,
) -> Result<()> {
    // Findings are per review, and planning the rest of its stack would cost
    // an agent call each for nothing printed.
    let stack = stack && !matches!(mode, Mode::Findings);
    // A local tour needs no forge.
    let detected = if local {
        None
    } else {
        Some(detect_review_provider()?)
    };
    let forge = detected
        .as_ref()
        .map(|(provider, review_provider)| (review_provider.as_ref(), provider.kind));
    let progress = Progress::start("looking up the review");
    let warnings = RefCell::new(Vec::new());
    let found = match forge {
        Some((review_provider, kind)) => {
            forge_stack(target, kind, review_provider, stack, &warnings)?
        }
        None => None,
    };
    let mut note = None;
    let (source, tour_stack) = match (found, forge) {
        (Some(found), Some((review_provider, kind))) => {
            (Source::Forge(kind, review_provider), found)
        }
        (_, forge) => {
            let branch = local_branch(target)?;
            if let Some((_, kind)) = forge {
                note = Some(format!(
                    "{branch} has no {kind} review yet, so it is toured from local branches"
                ));
            }
            (Source::Local, tour::stack::local(&branch, stack)?)
        }
    };
    let label = tour_stack.reviews[tour_stack.current].label();
    let tour = ReviewTour::load(source, tour_stack, planner, &progress)?;
    drop(progress);
    if let Some(note) = note {
        anstream::eprintln!("{}", style::dim(&note));
    }
    for warning in warnings.borrow().iter().chain(tour.warnings()) {
        anstream::eprintln!("{}", style::warn(&format!("warning: {warning}")));
    }
    let forge = match source {
        Source::Forge(kind, review_provider) => Some((review_provider, kind)),
        Source::Local => None,
    };

    if matches!(mode, Mode::Findings) {
        let findings = tour
            .findings()
            .context("the agent's review could not be used, so there are no findings to print")?;
        println!("{}", serde_json::to_string_pretty(&findings)?);
        return Ok(());
    }
    // A local branch has no review to mark seen.
    if forge.is_some() {
        tour::seen::record(
            (0..tour.review_count())
                .filter_map(|chapter| Some(tour.chapter_review(chapter)?.0.id.as_str())),
        );
    }
    let files = tour.file_count();
    let reviews = tour.review_count();
    let summary = format!(
        "{label} ({files} {}{})",
        if files == 1 { "file" } else { "files" },
        match (reviews, forge.is_some()) {
            (1, _) => String::new(),
            (_, true) => format!(", in a stack of {reviews} reviews"),
            (_, false) => format!(", in a stack of {reviews} branches"),
        }
    );

    let (path, open) = match mode {
        Mode::Serve { open } => return serve(&tour, forge, &summary, open),
        Mode::Snapshot { path, open } => (path, open),
        Mode::Findings => unreachable!("printed above"),
    };
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&path, tour.render(false))
        .with_context(|| format!("failed to write {}", path.display()))?;
    anstream::println!(
        "wrote a tour of {summary} to {}",
        style::dim(&path.display().to_string())
    );
    if open && let Err(error) = browser::open(&path) {
        anstream::eprintln!("could not open a browser ({error:#}); open the file above instead");
    }
    Ok(())
}

/// The review asked for and, with `stack`, the stack of reviews around it.
/// None for a branch with no review, which is toured locally instead.
fn forge_stack(
    target: Option<&str>,
    kind: ProviderKind,
    review_provider: &dyn ReviewProvider,
    stack: bool,
    warnings: &RefCell<Vec<String>>,
) -> Result<Option<tour::stack::Stack>> {
    let target = ReviewRef::parse(target, kind)?;
    // The open reviews, to find the stack in, come back while the target is
    // looked up.
    let (review, open_reviews) = thread::scope(|scope| {
        let open_reviews = stack.then(|| scope.spawn(|| review_provider.all_open_reviews()));
        let review = match target {
            ReviewRef::Number(number) => review_provider.review_by_number(number).map(Some),
            ReviewRef::Elsewhere { id, .. } => Err(ReviewRef::elsewhere(&id)),
            ReviewRef::Branch(branch) => {
                review_provider.review_for_branch_including_closed(&branch)
            }
        };
        let open_reviews = open_reviews.map(|handle| {
            handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        });
        (review, open_reviews)
    });
    let Some(review) = review? else {
        return Ok(None);
    };
    Ok(Some(match open_reviews {
        Some(Ok((open, complete))) => {
            if !complete {
                warnings.borrow_mut().push(format!(
                    "the repository has more open reviews than could be listed ({}), so the stack may be missing some",
                    open.len()
                ));
            }
            let default_branch = settings::remote()
                .ok()
                .and_then(|remote| git::remote_default_branch(&remote));
            tour::stack::around(
                review,
                open,
                &|branch, candidates| tour::same_named_parent(kind, branch, candidates, warnings),
                &|branch| tour::stack::long_lived(branch, default_branch.as_deref()),
            )
        }
        Some(Err(error)) => {
            warnings.borrow_mut().push(format!(
                "could not list open reviews to find the stack, so {} is toured alone: {error:#}",
                review.id
            ));
            tour::stack::Stack::single(review)
        }
        None => tour::stack::Stack::single(review),
    }))
}

/// The local branch a local tour is of: the one named, or the current one.
fn local_branch(target: Option<&str>) -> Result<String> {
    let Some(target) = target else {
        return git::current_branch();
    };
    if git::local_branches()?.iter().any(|branch| branch == target) {
        return Ok(target.to_owned());
    }
    bail!("{target} is not a local branch; a local tour is of a branch, not a review number or URL")
}

/// The port this repository's tours prefer, the same from one tour to the
/// next and unlikely to be another repository's.
fn preferred_port() -> u16 {
    let repository = git::git_common_path_absolute(".")
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    41_000 + (crate::tour::fnv1a(repository.as_bytes()) % 1_000) as u16
}

/// Serve the tour until the last tab showing it closes. A local tour, with no
/// `forge`, has nothing to post or refresh.
fn serve(
    tour: &ReviewTour,
    forge: Option<(&dyn ReviewProvider, ProviderKind)>,
    summary: &str,
    open: bool,
) -> Result<()> {
    let server = Server::bind(preferred_port())?;
    let url = server.url();
    anstream::println!(
        "touring {summary} at {}\nclose the page or press Ctrl-C to stop",
        style::dim(&url)
    );
    if open && let Err(error) = browser::open(&url) {
        anstream::eprintln!("could not open a browser ({error:#}); open the address above instead");
    }
    let page = tour.render(true);
    // One terminal prompt at a time, however many tabs submit at once.
    let prompting = Mutex::new(());
    server.run(&|request: Request| {
        let route = (request.method.as_str(), request.path.as_str());
        if route == ("GET", "") {
            return Response::html(page.clone());
        }
        let Some((provider, kind)) = forge else {
            return Response::error(404, "a local tour has no review to post to or refresh");
        };
        match route {
            ("POST", "api/submit") => match submit(tour, provider, kind, &request.body, &prompting)
            {
                Ok(Posted::Done(link)) => Response::json(&serde_json::json!({ "url": link })),
                Ok(Posted::Declined) => {
                    Response::error(409, "not posted: declined in the terminal")
                }
                Err(error) => Response::error(422, &format!("{error:#}")),
            },
            ("POST", "api/reply") => match reply(tour, provider, kind, &request.body, &prompting) {
                Ok(Posted::Done(_)) => Response::json(&serde_json::json!({})),
                Ok(Posted::Declined) => {
                    Response::error(409, "not posted: declined in the terminal")
                }
                Err(error) => Response::error(422, &format!("{error:#}")),
            },
            ("POST", "api/comment") => match comment(tour, provider, &request.body, &prompting) {
                Ok(Posted::Done(_)) => Response::json(&serde_json::json!({})),
                Ok(Posted::Declined) => {
                    Response::error(409, "not posted: declined in the terminal")
                }
                Err(error) => Response::error(422, &format!("{error:#}")),
            },
            ("POST", "api/resolve") => match resolve(tour, provider, &request.body) {
                Ok(()) => Response::json(&serde_json::json!({})),
                Err(error) => Response::error(422, &format!("{error:#}")),
            },
            ("GET", "api/conversation") => {
                match request
                    .param("chapter")
                    .and_then(|chapter| chapter.parse().ok())
                {
                    Some(chapter) => match tour.conversation(provider, chapter) {
                        Ok(found) => Response::json(&found),
                        Err(error) => Response::error(502, &format!("{error:#}")),
                    },
                    None => Response::error(400, "which review?"),
                }
            }
            _ => Response::error(404, "not found"),
        }
    })
}

enum Posted {
    Done(String),
    Declined,
}

/// Post the review a page submitted, once the terminal confirms it. The page
/// sends `{chapter, event, body, comments: [{path, oldPath, side, line,
/// startLine, pairedLine, body}]}`.
fn submit(
    tour: &ReviewTour,
    provider: &dyn ReviewProvider,
    kind: ProviderKind,
    request: &[u8],
    prompting: &Mutex<()>,
) -> Result<Posted> {
    let request: serde_json::Value =
        serde_json::from_slice(request).context("the review is not JSON")?;
    let chapter = request["chapter"]
        .as_u64()
        .and_then(|chapter| usize::try_from(chapter).ok())
        .context("which review?")?;
    let (review, head) = tour
        .chapter_review(chapter)
        .context("no such review in this tour")?;
    let event = request["event"]
        .as_str()
        .and_then(ReviewEvent::parse)
        .filter(|event| provider.review_events().contains(event))
        .context("that outcome is not available here")?;
    let comments = request["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .map(draft_comment)
        .collect::<Result<Vec<_>>>()?;
    let body = request["body"].as_str().unwrap_or_default().to_owned();
    if event == ReviewEvent::Comment && comments.is_empty() && body.trim().is_empty() {
        bail!("there is nothing to post");
    }
    if tour::head_moved(kind, provider, review, head)? {
        bail!(
            "{} has new commits since this tour started; restart the tour to review them",
            review.id
        );
    }
    let submission = Submission {
        event,
        body,
        comments,
        commit: head.to_owned(),
    };
    // The same review was sent before and never confirmed: its answer may
    // have been lost after it was posted, so look before posting it again.
    if let Some(since) = tour::submitted::pending(review, &submission) {
        let conversation = provider.conversation(review).with_context(|| {
            format!(
                "could not check whether the last attempt reached {}",
                review.id
            )
        })?;
        let me = provider
            .viewer()
            .context("could not tell who you are on the forge, to check the last attempt")?;
        match tour::submitted::landed(&submission, &conversation, since, &me) {
            tour::submitted::Landed::All => {
                tour::submitted::finish(review);
                anstream::println!(
                    "this review already reached {}; not posting it again",
                    review.id
                );
                return Ok(Posted::Done(review.url.clone()));
            }
            tour::submitted::Landed::Part => {
                tour::submitted::finish(review);
                bail!(
                    "part of this review already reached {}; check it there, then submit again to post all of it",
                    review.id
                );
            }
            tour::submitted::Landed::Unsure => {
                tour::submitted::finish(review);
                bail!(
                    "the discussion on {} is too long to check whether the last attempt reached it; check it there, then submit again to post it",
                    review.id
                );
            }
            tour::submitted::Landed::Nothing => {}
        }
    }

    let _turn = prompting
        .lock()
        .map_err(|_| anyhow!("the terminal prompt failed"))?;
    if !confirmed(
        &describe(review, &submission),
        &format!("post this review to {}?", review.id),
    )? {
        return Ok(Posted::Declined);
    }
    // Without a record, a lost answer could be posted twice, so refuse.
    tour::submitted::start(review, &submission)
        .context("could not record this review before posting it, so it was not posted")?;
    let link = provider.submit_review(review, &submission).map_err(|error| {
        anyhow!(
            "{error:#}; if it reached {} anyway, submitting the same review again checks before posting it twice",
            review.id
        )
    })?;
    tour::submitted::finish(review);
    anstream::println!("posted a review on {} {}", review.id, style::dim(&link));
    Ok(Posted::Done(link))
}

fn draft_comment(value: &serde_json::Value) -> Result<DraftComment> {
    let line = |key: &str| {
        value[key]
            .as_u64()
            .and_then(|line| u32::try_from(line).ok())
            .filter(|&line| line > 0)
    };
    let text = |key: &str| value[key].as_str().unwrap_or_default().to_owned();
    let body = text("body");
    if body.trim().is_empty() {
        bail!("a comment is empty");
    }
    Ok(DraftComment {
        path: text("path"),
        old_path: text("oldPath"),
        anchor: Anchor {
            side: if value["side"] == "old" {
                Side::Old
            } else {
                Side::New
            },
            line: line("line").context("a comment has no line")?,
            start_line: line("startLine"),
        },
        paired_line: line("pairedLine"),
        body,
    })
}

/// What a submission will post, for the terminal to confirm.
fn describe(review: &ReviewRequest, submission: &Submission) -> String {
    let count = submission.comments.len();
    let mut lines = vec![format!(
        "review of {}: {}{}",
        review.label(),
        match submission.event {
            ReviewEvent::Comment => "comment",
            ReviewEvent::Approve => "approve",
            ReviewEvent::RequestChanges => "request changes",
        },
        match count {
            0 => String::new(),
            1 => ", with 1 comment".to_owned(),
            count => format!(", with {count} comments"),
        }
    )];
    if !submission.body.trim().is_empty() {
        lines.push(format!("  summary: {}", first_line(&submission.body)));
    }
    for comment in &submission.comments {
        let side = if comment.anchor.side == Side::Old {
            " (old)"
        } else {
            ""
        };
        lines.push(format!(
            "  {}:{}{side}  {}",
            comment.path,
            comment.anchor.line,
            first_line(&comment.body)
        ));
    }
    lines.join("\n")
}

/// Show `what` in the terminal and ask `question` of it.
fn confirmed(what: &str, question: &str) -> Result<bool> {
    anstream::println!();
    anstream::println!("{what}");
    let yes = prompt::confirm(&format!("{question} [y/N] "))?;
    if !yes {
        anstream::println!("not posted");
    }
    Ok(yes)
}

/// The page's request body as JSON, with the review its `chapter` names and
/// the head the tour shows of it.
fn chapter_request<'a>(
    tour: &'a ReviewTour,
    request: &[u8],
) -> Result<(serde_json::Value, &'a ReviewRequest, &'a str)> {
    let request: serde_json::Value =
        serde_json::from_slice(request).context("the request is not JSON")?;
    let (review, head) = request["chapter"]
        .as_u64()
        .and_then(|chapter| usize::try_from(chapter).ok())
        .and_then(|chapter| tour.chapter_review(chapter))
        .context("no such review in this tour")?;
    Ok((request, review, head))
}

/// The thread a page refers to, by the fields the forges need to find it.
fn thread_from(value: &serde_json::Value) -> Result<Thread> {
    let line = value["line"]
        .as_u64()
        .and_then(|line| u32::try_from(line).ok())
        .filter(|&line| line > 0);
    Ok(Thread {
        id: value["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .context("which thread?")?
            .to_owned(),
        path: value["path"].as_str().unwrap_or_default().to_owned(),
        anchor: line.map(|line| Anchor {
            side: if value["side"] == "old" {
                Side::Old
            } else {
                Side::New
            },
            line,
            start_line: None,
        }),
        outdated: value["outdated"].as_bool().unwrap_or(false),
        resolved: None,
        diff_hunk: None,
        comments: Vec::new(),
    })
}

fn body_of(request: &serde_json::Value) -> Result<String> {
    let body = request["body"].as_str().unwrap_or_default();
    if body.trim().is_empty() {
        bail!("there is nothing to post");
    }
    Ok(body.to_owned())
}

/// The first line of `text` that says anything, shortened for one terminal
/// line.
fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    if line.chars().count() > 72 {
        format!("{}...", line.chars().take(69).collect::<String>())
    } else {
        line.to_owned()
    }
}

/// Reply to a thread once the terminal confirms it. The page sends
/// `{chapter, thread: {id, path, side, line, outdated}, body}`.
fn reply(
    tour: &ReviewTour,
    provider: &dyn ReviewProvider,
    kind: ProviderKind,
    request: &[u8],
    prompting: &Mutex<()>,
) -> Result<Posted> {
    let (request, review, head) = chapter_request(tour, request)?;
    let thread = thread_from(&request["thread"])?;
    let body = body_of(&request)?;
    // A forge that places a reply by line needs the lines to be current.
    if kind == ProviderKind::Gitea && tour::head_moved(kind, provider, review, head)? {
        bail!(
            "{} has new commits since this tour started; restart the tour to reply",
            review.id
        );
    }
    let place = match thread.anchor {
        Some(anchor) => format!("{}:{}", thread.path, anchor.line),
        None => thread.path.clone(),
    };
    let _turn = prompting
        .lock()
        .map_err(|_| anyhow!("the terminal prompt failed"))?;
    let what = format!(
        "reply on {} at {place}:\n  {}",
        review.label(),
        first_line(&body)
    );
    if !confirmed(&what, &format!("post this reply to {}?", review.id))? {
        return Ok(Posted::Declined);
    }
    provider.reply_to_thread(review, &thread, head, &body)?;
    anstream::println!("replied on {} at {place}", review.id);
    Ok(Posted::Done(String::new()))
}

/// Comment on a review as a whole once the terminal confirms it. The page
/// sends `{chapter, body}`.
fn comment(
    tour: &ReviewTour,
    provider: &dyn ReviewProvider,
    request: &[u8],
    prompting: &Mutex<()>,
) -> Result<Posted> {
    let (request, review, _) = chapter_request(tour, request)?;
    let body = body_of(&request)?;
    let _turn = prompting
        .lock()
        .map_err(|_| anyhow!("the terminal prompt failed"))?;
    let what = format!("comment on {}:\n  {}", review.label(), first_line(&body));
    if !confirmed(&what, &format!("post this comment to {}?", review.id))? {
        return Ok(Posted::Declined);
    }
    provider.comment_on_review(review, &body)?;
    anstream::println!("commented on {}", review.id);
    Ok(Posted::Done(String::new()))
}

/// Resolve a thread, or open it again. The page sends `{chapter, thread,
/// resolved}`. Nothing is posted, so nothing is asked; the terminal notes it.
fn resolve(tour: &ReviewTour, provider: &dyn ReviewProvider, request: &[u8]) -> Result<()> {
    let (request, review, _) = chapter_request(tour, request)?;
    let thread = thread_from(&request["thread"])?;
    let resolved = request["resolved"]
        .as_bool()
        .context("resolve or reopen?")?;
    provider.set_resolved(review, &thread, resolved)?;
    anstream::println!(
        "{} a thread on {} at {}",
        if resolved { "resolved" } else { "reopened" },
        review.id,
        thread.path
    );
    Ok(())
}

/// How the tour is planned: None for a standard tour; for an agent tour, its
/// agent and any plan given ahead of time. Asks, once per repository, before
/// an agent is first sent this repository's reviews.
fn planner(
    style: Option<TourStyle>,
    agent: Option<&str>,
    plan: Option<&Path>,
    refresh: bool,
    ask: bool,
) -> Result<Option<Planner>> {
    let style = match (style, plan) {
        (Some(style), _) => style,
        (None, Some(_)) => TourStyle::AgentTour,
        (None, None) => settings::tour_style()?,
    };
    if style == TourStyle::Standard {
        if plan.is_some() {
            bail!("--plan lays out an agent tour; drop --style standard to use it");
        }
        return Ok(None);
    }
    let plan_file = plan
        .map(|path| {
            std::fs::read_to_string(path)
                .with_context(|| format!("failed to read the plan {}", path.display()))
        })
        .transpose()?;
    let command = match agent {
        Some(command) => Some(command.to_owned()),
        None => git::config_get(settings::TOUR_AGENT_KEY)?,
    }
    .filter(|command| !command.trim().is_empty());
    let agent = command.as_deref().map(Agent::parse).transpose()?;
    match &agent {
        Some(agent) => allow_agent(agent, ask)?,
        None if plan_file.is_none() => bail!(
            "an agent tour needs an agent to plan it: set {} (for example to `claude -p`) or pass --agent",
            settings::TOUR_AGENT_KEY
        ),
        None => {}
    }
    Ok(Some(Planner {
        agent,
        plan_file,
        refresh,
        review: style == TourStyle::AgentReview,
    }))
}

/// Make sure the user has agreed to send this repository's reviews to
/// `agent`, asking the first time and remembering the command agreed to: a
/// different command, which may send them somewhere else, asks again. Without
/// `ask`, as when stdout is data for another program, it only says how.
fn allow_agent(agent: &Agent, ask: bool) -> Result<()> {
    let command = agent.display();
    let allowed = git::config_get(settings::TOUR_AGENT_ALLOWED_KEY)?;
    if allowed.as_deref().map(str::trim) == Some(command.as_str()) {
        return Ok(());
    }
    let before = allowed
        .filter(|before| !before.trim().is_empty())
        .map(|before| format!(" Agent tours here were allowed with `{}`.", before.trim()))
        .unwrap_or_default();
    if !ask || !std::io::stdin().is_terminal() {
        bail!(
            "an agent tour sends each review's description, commits, and diff to `{command}`.{before} To allow that in this repository, run the tour once in a terminal or set `git config {} \"{command}\"`",
            settings::TOUR_AGENT_ALLOWED_KEY
        );
    }
    anstream::println!(
        "An agent tour sends each review's description, commits, and diff to `{command}`, which may pass them to its model provider.{before}"
    );
    if !prompt::confirm(&format!(
        "Allow agent tours with `{command}` in this repository? [y/N] "
    ))? {
        bail!(
            "no agent tour without your agreement; run with --style standard for a tour by file role"
        );
    }
    git::config_set(settings::TOUR_AGENT_ALLOWED_KEY, &command)?;
    Ok(())
}
