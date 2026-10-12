use std::path::PathBuf;

use std::cell::RefCell;
use std::sync::Mutex;
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use clap::ArgAction;
use clap_complete::engine::ArgValueCompleter;

use crate::browser;
use crate::commands::Run;
use crate::completions;
use crate::git;
use crate::progress::Progress;
use crate::prompt;
use crate::providers::{
    Anchor, DraftComment, ProviderKind, ReviewEvent, ReviewProvider, ReviewRef, ReviewRequest,
    Side, Submission, detect_review_provider,
};
use crate::settings;
use crate::style;
use crate::tour::server::{Request, Response, Server};
use crate::tour::{self, Tour as ReviewTour};

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
    /// Print the tour's address without opening a browser.
    #[arg(long, action = ArgAction::SetTrue)]
    no_open: bool,
    /// Write a read-only snapshot of the tour to this file instead of
    /// serving it.
    #[arg(long, short = 'o', value_name = "PATH")]
    output: Option<PathBuf>,
}

impl Run for Tour {
    fn run(self) -> Result<()> {
        write_tour(
            self.target.as_deref(),
            self.output,
            !self.no_open,
            !self.no_stack,
        )
    }
}

fn write_tour(
    target: Option<&str>,
    output: Option<PathBuf>,
    open: bool,
    stack: bool,
) -> Result<()> {
    let (provider, review_provider) = detect_review_provider()?;
    let target = ReviewRef::parse(target, provider.kind)?;
    let progress = Progress::start(match &target {
        ReviewRef::Number(number) => format!("looking up review {number}"),
        ReviewRef::Branch(branch) => format!("looking up the review for {branch}"),
    });
    let review_provider = review_provider.as_ref();
    // The open reviews, to find the stack in, come back while the target is
    // looked up.
    let (review, open_reviews) = thread::scope(|scope| {
        let open_reviews = stack.then(|| scope.spawn(|| review_provider.all_open_reviews()));
        let review = match target {
            ReviewRef::Number(number) => review_provider.review_by_number(number),
            ReviewRef::Branch(branch) => review_provider
                .review_for_branch_including_closed(&branch)
                .and_then(|review| {
                    review.with_context(|| {
                        format!(
                            "no {} review found for {branch}; submit it first with `git stk submit`",
                            provider.kind
                        )
                    })
                }),
        };
        let open_reviews = open_reviews.map(|handle| {
            handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        });
        (review, open_reviews)
    });
    let review = review?;

    let label = review.label();
    let warnings = RefCell::new(Vec::new());
    let stack = match open_reviews {
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
                &|branch, candidates| {
                    tour::same_named_parent(provider.kind, branch, candidates, &warnings)
                },
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
    };
    let tour = ReviewTour::load(provider.kind, review_provider, stack, &progress)?;
    drop(progress);
    for warning in warnings.borrow().iter().chain(tour.warnings()) {
        anstream::eprintln!("{}", style::warn(&format!("warning: {warning}")));
    }
    let files = tour.file_count();
    let reviews = tour.review_count();
    let summary = format!(
        "{label} ({files} {}{})",
        if files == 1 { "file" } else { "files" },
        if reviews > 1 {
            format!(", in a stack of {reviews} reviews")
        } else {
            String::new()
        }
    );

    let Some(path) = output else {
        return serve(&tour, review_provider, provider.kind, &summary, open);
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

/// Serve the tour until the last tab showing it closes.
fn serve(
    tour: &ReviewTour,
    provider: &dyn ReviewProvider,
    kind: ProviderKind,
    summary: &str,
    open: bool,
) -> Result<()> {
    let server = Server::bind()?;
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
    server.run(
        &|request: Request| match (request.method.as_str(), request.path.as_str()) {
            ("GET", "") => Response::html(page.clone()),
            ("POST", "api/submit") => match submit(tour, provider, kind, &request.body, &prompting)
            {
                Ok(Posted::Done(link)) => Response::json(&serde_json::json!({ "url": link })),
                Ok(Posted::Declined) => {
                    Response::error(409, "not posted: declined in the terminal")
                }
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
        },
    )
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
    if tour::head_moved(kind, review, head)? {
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

    let _turn = prompting
        .lock()
        .map_err(|_| anyhow!("the terminal prompt failed"))?;
    anstream::println!();
    anstream::println!("{}", describe(review, &submission));
    if !prompt::confirm(&format!("post this review to {}? [y/N] ", review.id))? {
        anstream::println!("not posted");
        return Ok(Posted::Declined);
    }
    let link = provider.submit_review(review, &submission)?;
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
    let first_line = |text: &str| {
        let line = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default();
        if line.chars().count() > 72 {
            format!("{}...", line.chars().take(69).collect::<String>())
        } else {
            line.to_owned()
        }
    };
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
