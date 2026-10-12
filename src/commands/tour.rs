use std::path::PathBuf;

use std::cell::RefCell;
use std::thread;

use anyhow::{Context, Result};
use clap::ArgAction;
use clap_complete::engine::ArgValueCompleter;

use crate::browser;
use crate::commands::Run;
use crate::completions;
use crate::git;
use crate::progress::Progress;
use crate::providers::{ReviewProvider, ReviewRef, detect_review_provider};
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
        return serve(&tour, review_provider, &summary, open);
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
    server.run(
        &|request: Request| match (request.method.as_str(), request.path.as_str()) {
            ("GET", "") => Response::html(page.clone()),
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
