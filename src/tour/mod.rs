//! A review's changes as a self-contained HTML page that walks through them
//! step by step, for `git stk tour`.

use std::cell::RefCell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::agent::Agent;
use crate::git;
use crate::notes;
use crate::progress::Progress;
use crate::providers::{Conversation, ProviderKind, ReviewProvider, ReviewRequest};
use crate::settings;

mod conversation;
mod diff;
mod markdown;
pub mod plan;
pub mod server;
pub mod stack;
mod steps;

use stack::Stack;

use diff::{FileDiff, FileStatus, LineKind};
use steps::Step;

const TEMPLATE: &str = include_str!("template.html");
const HIGHLIGHT_JS: &str = include_str!("vendor/highlight.min.js");
const HIGHLIGHT_LICENSE: &str = include_str!("vendor/highlight.js-LICENSE");

/// The template's placeholders, in the order they appear in it.
const TITLE_PLACEHOLDER: &str = "<!--TOUR_TITLE-->";
const LICENSE_PLACEHOLDER: &str = "@@HIGHLIGHT_LICENSE@@";
const HIGHLIGHT_PLACEHOLDER: &str = "/*HIGHLIGHT_JS*/";
const DATA_PLACEHOLDER: &str = "/*TOUR_DATA*/null";

/// Files larger than this are shown without their full text, so without
/// whole-file highlighting or context expansion.
const MAX_EMBEDDED_BYTES: u64 = 1024 * 1024;

/// Once a tour has embedded this much text, later files go without, keeping
/// the page quick to open on a large review or stack.
const MAX_TOTAL_EMBEDDED_BYTES: u64 = 16 * 1024 * 1024;

/// A review, or the stack of reviews it belongs to, read from git; each
/// review is a chapter of the tour.
pub struct Tour {
    chapters: Vec<Chapter>,
    current: usize,
    warnings: Vec<String>,
    /// The outcomes a review can be submitted with on this forge.
    events: Vec<&'static str>,
    /// Whether this forge resolves threads.
    resolves: bool,
    /// The forge, for writing a comment in its own Markdown.
    forge: ProviderKind,
}

/// One review's commits and changes.
struct Chapter {
    review: ReviewRequest,
    /// The chapter this review is stacked on, if it is in the tour.
    parent: Option<usize>,
    body: String,
    base_sha: String,
    head_sha: String,
    commits: Vec<(String, String)>,
    steps: Vec<Step>,
    /// The review's discussion as page data.
    conversation: Value,
    /// The review's discussion for the tour agent, or null.
    discussion: Value,
    /// The repository's instructions for contributors, for the tour agent.
    guidance: Vec<(String, String)>,
    /// How the review's CI stands, for an agent review.
    ci: &'static str,
    /// The agent's review of an earlier head of this review, to follow up on.
    previous: Option<plan::Previous>,
    /// The plan the review's steps follow, for an agent tour.
    plan: Option<PlanInfo>,
}

/// Where a chapter's plan came from and what it adds beyond its steps.
struct PlanInfo {
    overview: String,
    notes: Vec<plan::Note>,
    findings: Vec<plan::Finding>,
    assessment: Option<plan::Assessment>,
    follow_up: Option<plan::FollowUp>,
    reported: plan::Reported,
    /// The agent command that wrote it, or None for a plan given as a file.
    command: Option<String>,
    /// Saved from an earlier tour of the same head rather than asked for now.
    cached: bool,
}

/// How an agent tour gets the plans its reviews follow.
pub struct Planner {
    pub agent: Option<Agent>,
    /// A plan written ahead of time for the review asked for.
    pub plan_file: Option<String>,
    /// Ask the agent again rather than use plans saved from before.
    pub refresh: bool,
    /// Ask for review findings with the plan (`agentReview`).
    pub review: bool,
}

/// Changesets planned at once.
const PLANNING_AT_ONCE: usize = 4;

impl Tour {
    /// Read `stack`. A review that cannot be fetched or read, or that
    /// matched its parent only by branch name, is left out with the reviews
    /// stacked on it; one with no changes of its own is not shown.
    pub fn load(
        kind: ProviderKind,
        provider: &dyn ReviewProvider,
        stack: Stack,
        planner: Option<&Planner>,
        progress: &Progress,
    ) -> Result<Self> {
        let Stack {
            reviews,
            parents,
            current,
        } = stack;
        progress.set(match reviews.len() {
            1 => format!("fetching {}", reviews[0].id),
            count => format!("fetching {count} reviews"),
        });
        let mut warnings = Vec::new();
        // The descriptions and discussions come from the provider while git
        // fetches.
        let (tips, bodies, conversations) = thread::scope(|scope| {
            let bodies: Vec<_> = reviews
                .iter()
                .map(|review| scope.spawn(move || provider.review_body(review)))
                .collect();
            let conversations: Vec<_> = reviews
                .iter()
                .map(|review| scope.spawn(move || provider.conversation(review)))
                .collect();
            let tips = stack_tips(kind, &reviews, current, &mut warnings);
            (tips, joined(bodies), joined(conversations))
        });
        let tips = tips?;
        let linked: Vec<bool> = parents
            .iter()
            .zip(&tips)
            .map(|(parent, tip)| match (parent, tip) {
                (Some(parent), Some((base_tip, _))) => tips[*parent]
                    .as_ref()
                    .is_some_and(|(_, head)| head == base_tip),
                _ => false,
            })
            .collect();
        let reachable = reachable_from_root(
            &parents,
            current,
            |index| tips[index].is_some(),
            |index| linked[index],
        );

        progress.set("reading changes");
        let mut budget = MAX_TOTAL_EMBEDDED_BYTES;
        let mut reviews: Vec<Option<ReviewRequest>> = reviews.into_iter().map(Some).collect();
        let mut tips = tips;
        let mut bodies: Vec<Option<Result<String>>> = bodies.into_iter().map(Some).collect();
        let mut conversations: Vec<Option<Result<Conversation>>> =
            conversations.into_iter().map(Some).collect();
        let mut read: Vec<Option<Option<Chapter>>> = (0..reviews.len()).map(|_| None).collect();
        // The asked-for review first, so the text budget goes to it.
        let order = std::iter::once(current).chain((0..reviews.len()).filter(|&i| i != current));
        for index in order.filter(|&index| reachable[index]) {
            let (Some(review), Some((base_tip, head_sha)), Some(body)) = (
                reviews[index].take(),
                tips[index].take(),
                bodies[index].take(),
            ) else {
                continue;
            };
            let id = review.id.clone();
            let base = review.base.clone();
            let chapter = body
                .with_context(|| format!("failed to read the description of {id}"))
                .and_then(|body| Chapter::read(review, &body, &base_tip, head_sha, &mut budget));
            let chapter = chapter.map(|chapter| {
                chapter.map(|mut chapter| {
                    chapter.conversation = match conversations[index].take() {
                        Some(Ok(conversation)) => {
                            chapter.discussion = conversation::for_agent(&conversation);
                            conversation::data(&conversation)
                        }
                        Some(Err(error)) => {
                            warnings
                                .push(format!("could not read the discussion on {id}: {error:#}"));
                            conversation::error(&format!("{error:#}"))
                        }
                        None => conversation::error("not read"),
                    };
                    chapter
                })
            });
            match chapter {
                Ok(None) if index == current => bail!(
                    "{id} has no changes against {base}; a merged review's changes are already in its base"
                ),
                Ok(chapter) => read[index] = Some(chapter),
                Err(error) if index != current => warnings.push(format!(
                    "could not read {id}, so it is left out with the reviews stacked on it: {error:#}"
                )),
                Err(error) => return Err(error),
            }
        }

        let included = reachable_from_root(
            &parents,
            current,
            |index| read[index].is_some(),
            |index| linked[index],
        );
        let mut chapters: Vec<Chapter> = Vec::new();
        let mut chapter_of: Vec<Option<usize>> = vec![None; parents.len()];
        let mut current_chapter = 0;
        for index in 0..parents.len() {
            if !included[index] {
                continue;
            }
            // A review with no changes of its own is skipped, and the ones
            // stacked on it hang from its parent.
            let Some(Some(mut chapter)) = read[index].take() else {
                chapter_of[index] = parents[index].and_then(|parent| chapter_of[parent]);
                continue;
            };
            chapter.parent = parents[index].and_then(|parent| chapter_of[parent]);
            if index == current {
                current_chapter = chapters.len();
            }
            chapter_of[index] = Some(chapters.len());
            chapters.push(chapter);
        }
        if let Some(planner) = planner {
            plan_chapters(
                &mut chapters,
                current_chapter,
                planner,
                provider,
                progress,
                &mut warnings,
            );
        }
        Ok(Self {
            chapters,
            current: current_chapter,
            warnings,
            events: provider
                .review_events()
                .iter()
                .map(|event| event.as_str())
                .collect(),
            resolves: provider.resolves_threads(),
            forge: kind,
        })
    }

    /// What was left out of the tour and why.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    fn current(&self) -> &Chapter {
        &self.chapters[self.current]
    }

    /// How many reviews the tour covers.
    pub fn review_count(&self) -> usize {
        self.chapters.len()
    }

    /// How many files the review asked for changes.
    pub fn file_count(&self) -> usize {
        self.current()
            .steps
            .iter()
            .map(|step| step.files.len())
            .sum()
    }

    /// The page. A `served` page talks to the tour server; one that is not
    /// is a read-only snapshot.
    pub fn render(&self, served: bool) -> String {
        let title = format!("Tour of {}", self.current().review.label());
        fill(
            TEMPLATE,
            &[
                (TITLE_PLACEHOLDER, &escape_html(&title)),
                (LICENSE_PLACEHOLDER, HIGHLIGHT_LICENSE),
                (HIGHLIGHT_PLACEHOLDER, HIGHLIGHT_JS),
                (DATA_PLACEHOLDER, &script_json(&self.data(served))),
            ],
        )
    }

    /// A chapter's review and the head the tour shows of it.
    pub fn chapter_review(&self, chapter: usize) -> Option<(&ReviewRequest, &str)> {
        self.chapters
            .get(chapter)
            .map(|chapter| (&chapter.review, chapter.head_sha.as_str()))
    }

    /// A chapter's discussion as it stands now, for a page refreshing it.
    pub fn conversation(&self, provider: &dyn ReviewProvider, chapter: usize) -> Result<Value> {
        let chapter = self
            .chapters
            .get(chapter)
            .context("no such review in this tour")?;
        Ok(conversation::data(&provider.conversation(&chapter.review)?))
    }

    fn data(&self, served: bool) -> Value {
        json!({
            "served": served,
            "reviewEvents": self.events,
            "resolvesThreads": self.resolves,
            "forge": self.forge.to_string(),
            "current": self.current,
            "chapters": self.chapters.iter().map(Chapter::data).collect::<Vec<_>>(),
        })
    }
}

impl Chapter {
    /// Read the change from the merge base of `base_tip` and `head_sha` to
    /// `head_sha`, embedding file texts from what is left of `budget`. None
    /// when the review has no changes.
    fn read(
        review: ReviewRequest,
        body: &str,
        base_tip: &str,
        head_sha: String,
        budget: &mut u64,
    ) -> Result<Option<Self>> {
        let base_sha = git::merge_base(base_tip, &head_sha)?;
        let files = diff::parse(&git::tour_diff(&base_sha, &head_sha)?);
        if files.is_empty() {
            return Ok(None);
        }

        let paths: Vec<&str> = files.iter().map(FileDiff::path).collect();
        let generated = git::generated_paths(&paths, &head_sha)?;
        let mut steps = steps::group(files, |path| generated.iter().any(|g| g == path));
        let embedded = embed_texts(&mut steps, &base_sha, &head_sha, *budget)?;
        *budget = budget.saturating_sub(embedded);

        let mut commits = git::log_oneline(&format!("{base_sha}..{head_sha}"))?;
        commits.reverse();

        Ok(Some(Self {
            review,
            parent: None,
            body: notes::readable_body(body),
            base_sha,
            head_sha,
            commits,
            steps,
            conversation: Value::Null,
            discussion: Value::Null,
            guidance: Vec::new(),
            ci: "unknown",
            previous: None,
            plan: None,
        }))
    }

    fn data(&self) -> Value {
        json!({
            "parent": self.parent,
            "review": {
                "id": self.review.id,
                "title": self.review.title,
                "url": self.review.url,
                "state": self.review.state.to_string(),
                "branch": self.review.branch,
                "base": self.review.base,
                "bodyHtml": markdown::to_html(&self.body),
            },
            "conversation": self.conversation,
            "plan": self.plan.as_ref().map(|plan| json!({
                "source": if plan.command.is_some() { "agent" } else { "file" },
                "command": plan.command,
                "cached": plan.cached,
                "provider": plan.reported.provider,
                "model": plan.reported.model,
                "overviewHtml": markdown::to_html(&plan.overview),
                "notes": plan.notes.iter().map(|note| json!({
                    "path": note.path,
                    "side": note.side.as_str(),
                    "line": note.line,
                    "bodyHtml": markdown::to_html(&note.body),
                })).collect::<Vec<_>>(),
                "findings": plan.findings.iter().map(|finding| json!({
                    "path": finding.path,
                    "side": finding.side.as_str(),
                    "line": finding.line,
                    "startLine": finding.start_line,
                    "severity": finding.severity.as_str(),
                    "category": finding.category.map(plan::Category::as_str),
                    "title": finding.title,
                    // The Markdown too, for turning a finding into a comment.
                    "body": finding.body,
                    "bodyHtml": markdown::to_html(&finding.body),
                    "suggestion": finding.suggestion,
                    "evidenceHtml": markdown::to_html(&finding.evidence),
                    "preExisting": finding.pre_existing,
                })).collect::<Vec<_>>(),
                "followUp": plan.follow_up.as_ref().map(|follow_up| json!({
                    "head": follow_up.head,
                    "confidence": follow_up.confidence,
                    "findings": follow_up.findings.iter().map(|(finding, status, note)| json!({
                        "path": finding.path,
                        "line": finding.line,
                        "severity": finding.severity,
                        "title": finding.title,
                        "status": status.as_str(),
                        "note": note,
                    })).collect::<Vec<_>>(),
                })),
                "assessment": plan.assessment.as_ref().map(|assessment| json!({
                    "confidence": assessment.confidence,
                    "risk": assessment.risk.map(plan::Risk::as_str),
                    "rationale": assessment.rationale,
                    "questionHtml": assessment.question.as_deref().map(markdown::to_html),
                    "question": assessment.question,
                })),
            })),
            "baseSha": self.base_sha,
            "headSha": self.head_sha,
            "commits": self.commits.iter().map(|(sha, subject)| {
                json!({ "sha": sha, "subject": subject })
            }).collect::<Vec<_>>(),
            "steps": self.steps.iter().map(|step| json!({
                "id": step.id(),
                "title": step.title(),
                "summary": step.label.as_ref().map(|label| markdown::to_html(&label.summary)),
                "collapsed": step.kind == steps::StepKind::Generated,
                "files": step.files.iter().map(file_data).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        })
    }
}

/// Plan each chapter: the asked-for one from a plan file when there is one,
/// the rest from a saved plan or the agent, asked a few at a time. A chapter
/// whose plan cannot be had or used keeps its steps by file role.
fn plan_chapters(
    chapters: &mut [Chapter],
    current: usize,
    planner: &Planner,
    provider: &dyn ReviewProvider,
    progress: &Progress,
    warnings: &mut Vec<String>,
) {
    // The answer each chapter's plan comes from, and whether it was saved.
    let mut answers: Vec<Option<(Result<String>, bool)>> =
        (0..chapters.len()).map(|_| None).collect();
    if let Some(text) = &planner.plan_file {
        answers[current] = Some((Ok(text.clone()), false));
    }
    let mut to_ask = Vec::new();
    if let Some(agent) = &planner.agent {
        let mut previous: Vec<Option<plan::Previous>> = (0..chapters.len()).map(|_| None).collect();
        for (index, chapter) in chapters.iter().enumerate() {
            if answers[index].is_some() {
                continue;
            }
            let path = (!planner.refresh)
                .then(|| plan_cache(chapter, agent, planner.review).ok())
                .flatten();
            let saved = path
                .as_ref()
                .and_then(|path| std::fs::read_to_string(path).ok());
            if planner.review {
                // A saved plan keeps the review it followed up on. A new one
                // follows up on the latest review of another head, and a head
                // reviewed before only on those reviewed before it was.
                previous[index] = match (&saved, &path) {
                    (Some(_), Some(path)) => followed_review(path),
                    _ => {
                        let own = plan_cache(chapter, agent, planner.review)
                            .ok()
                            .and_then(|path| first_reviewed(&path));
                        previous_review(chapter, own)
                    }
                };
            }
            match saved {
                Some(answer) => answers[index] = Some((Ok(answer), true)),
                None => to_ask.push(index),
            }
        }
        for (chapter, previous) in chapters.iter_mut().zip(previous) {
            chapter.previous = previous;
        }
        if planner.review {
            // How CI stands, so the agent need not report what it checks;
            // asked only for the reviews about to be planned.
            let states: Vec<&'static str> = thread::scope(|scope| {
                let asked: Vec<_> = to_ask
                    .iter()
                    .map(|&index| {
                        let review = &chapters[index].review;
                        scope.spawn(move || {
                            provider.check_status(review).map_or("unknown", ci_state)
                        })
                    })
                    .collect();
                joined(asked)
            });
            for (&index, state) in to_ask.iter().zip(states) {
                chapters[index].ci = state;
            }
        }
        // A plan made without the guidance is used but not saved.
        let mut unsaved = Vec::new();
        for &index in &to_ask {
            match read_guidance(&chapters[index]) {
                Ok(guidance) => chapters[index].guidance = guidance,
                Err(error) => {
                    warnings.push(format!(
                        "could not read the repository's guidance for {}, so it is planned without it: {error:#}",
                        chapters[index].review.id
                    ));
                    unsaved.push(index);
                }
            }
        }
        let prompts: Vec<(usize, String)> = to_ask
            .iter()
            .map(|&index| (index, plan_request(chapters, index).prompt(planner.review)))
            .collect();
        let cached = answers
            .iter()
            .filter(|answer| matches!(answer, Some((_, true))))
            .count();
        // Each usable answer is saved as it arrives, so an interrupted tour
        // keeps the plans it already has.
        let save = |index: usize, answer: &str| {
            let chapter = &chapters[index];
            let usable = !unsaved.contains(&index)
                && plan::parse(answer)
                    .and_then(|parsed| plan::check(&parsed, &chapter.steps))
                    .is_ok();
            if usable && let Ok(path) = plan_cache(chapter, agent, planner.review) {
                let _ = path
                    .parent()
                    .map(std::fs::create_dir_all)
                    .map(|_| std::fs::write(&path, answer));
            }
        };
        for (index, result) in ask_in_pool(agent, &prompts, cached, progress, save) {
            answers[index] = Some((result, false));
        }
    }

    for (index, answer) in answers.into_iter().enumerate() {
        let Some((answer, cached)) = answer else {
            continue;
        };
        let chapter = &mut chapters[index];
        let from_file = index == current && planner.plan_file.is_some();
        let source = if from_file {
            "the plan file".to_owned()
        } else {
            planner
                .agent
                .as_ref()
                .map_or_else(String::new, |agent| format!("`{}`", agent.display()))
        };
        let applied = answer.and_then(|answer| {
            let mut parsed = plan::parse(&answer)?;
            plan::check(&parsed, &chapter.steps)?;
            if !planner.review && !from_file {
                parsed.findings.clear();
                parsed.assessment = plan::RawAssessment::default();
            }
            let statuses = std::mem::take(&mut parsed.statuses);
            let steps = std::mem::take(&mut chapter.steps);
            let mut applied = plan::apply(parsed, steps);
            let follow_up = chapter
                .previous
                .as_ref()
                .filter(|_| planner.review && !from_file)
                .map(|previous| plan::follow_up(previous, statuses, &mut applied.corrections));
            Ok((applied, follow_up))
        });
        match applied {
            Ok((applied, follow_up)) => {
                // What this review showed, for the next head's to follow up on.
                if planner.review
                    && !from_file
                    && !cached
                    && let Some(agent) = &planner.agent
                    && let Ok(path) = plan_cache(chapter, agent, planner.review)
                {
                    let mut shown =
                        plan::Previous::shown(&applied.findings, applied.assessment.as_ref());
                    shown["followed"] = chapter
                        .previous
                        .as_ref()
                        .map_or(Value::Null, plan::Previous::to_json);
                    // When this head was first reviewed, kept through a refresh.
                    shown["reviewedAt"] = first_reviewed(&path).unwrap_or_else(now_millis).into();
                    let _ = std::fs::write(path.with_extension(SHOWN_EXTENSION), shown.to_string());
                }
                if !applied.corrections.is_empty() {
                    warnings.push(format!(
                        "the plan for {} from {source} {}; that was set right",
                        chapter.review.id,
                        applied.corrections.join("; ")
                    ));
                }
                chapter.steps = applied.steps;
                chapter.plan = Some(PlanInfo {
                    overview: applied.overview,
                    notes: applied.notes,
                    findings: applied.findings,
                    assessment: applied.assessment,
                    follow_up,
                    reported: applied.reported,
                    command: (!from_file)
                        .then(|| planner.agent.as_ref().map(Agent::display))
                        .flatten(),
                    cached,
                });
            }
            Err(error) => warnings.push(format!(
                "could not plan {} with {source}, so it is toured by file role: {error:#}",
                chapter.review.id
            )),
        }
    }
}

/// Ask `agent` each of `prompts`, a few at a time: each worker takes the next
/// as soon as it is free, so one slow changeset holds up no others. The
/// terminal counts them off as they come back, along with the `cached`
/// changesets whose saved plans need no asking.
fn ask_in_pool(
    agent: &Agent,
    prompts: &[(usize, String)],
    cached: usize,
    progress: &Progress,
    on_answer: impl Fn(usize, &str) + Sync,
) -> Vec<(usize, Result<String>)> {
    let total = prompts.len() + cached;
    let report = |finished: usize| {
        let saved = if cached > 0 {
            format!(" ({cached} cached)")
        } else {
            String::new()
        };
        progress.set(format!("agent reviews: {finished}/{total} done{saved}"));
    };
    if total > 0 {
        report(cached);
    }
    if prompts.is_empty() {
        return Vec::new();
    }
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(cached);
    let answers = Mutex::new(Vec::with_capacity(prompts.len()));
    thread::scope(|scope| {
        for _ in 0..PLANNING_AT_ONCE.min(prompts.len()) {
            scope.spawn(|| {
                loop {
                    let taken = next.fetch_add(1, Ordering::Relaxed);
                    let Some((index, prompt)) = prompts.get(taken) else {
                        break;
                    };
                    let answer = agent.ask(prompt);
                    if let Ok(text) = &answer {
                        on_answer(*index, text);
                    }
                    if let Ok(mut answers) = answers.lock() {
                        answers.push((*index, answer));
                    }
                    report(done.fetch_add(1, Ordering::Relaxed) + 1);
                }
            });
        }
    });
    answers.into_inner().unwrap_or_default()
}

/// What chapter `index` offers its planner.
fn plan_request(chapters: &[Chapter], index: usize) -> plan::Request<'_> {
    let chapter = &chapters[index];
    let mut below = Vec::new();
    let mut parent = chapter.parent;
    while let Some(at) = parent {
        below.push(chapters[at].review.label());
        parent = chapters[at].parent;
    }
    below.reverse();
    plan::Request {
        review: &chapter.review,
        body: &chapter.body,
        commits: &chapter.commits,
        steps: &chapter.steps,
        below,
        guidance: &chapter.guidance,
        discussion: &chapter.discussion,
        ci: chapter.ci,
        previous: chapter.previous.as_ref(),
    }
}

/// Beside each saved review plan, what the review showed once checked.
const SHOWN_EXTENSION: &str = "shown.json";

/// The latest saved agent review of another head of `chapter`'s review, from
/// any agent, first reviewed before `before` when given, for a follow-up.
fn previous_review(chapter: &Chapter, before: Option<u64>) -> Option<plan::Previous> {
    let dir = git::git_common_path_absolute("stk/tours/plans").ok()?;
    let prefix = format!("{}-", chapter.review.id_value());
    let head = chapter.head_sha.get(..12).unwrap_or(&chapter.head_sha);
    let mut saved: Vec<(u64, String, Value)> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.ends_with(&format!(".{SHOWN_EXTENSION}"))
                .then_some(())?;
            let earlier = name.strip_prefix(&prefix)?.split('-').next()?.to_owned();
            let shown: Value =
                serde_json::from_str(&std::fs::read_to_string(entry.path()).ok()?).ok()?;
            let reviewed = shown["reviewedAt"].as_u64()?;
            let in_time = before.is_none_or(|before| reviewed < before);
            (earlier != head && in_time).then_some((reviewed, earlier, shown))
        })
        .collect();
    saved.sort_by_key(|(reviewed, _, _)| std::cmp::Reverse(*reviewed));
    saved
        .into_iter()
        .find_map(|(_, earlier, shown)| plan::Previous::from_shown(&earlier, &shown))
}

/// When the head of the plan saved at `plan` was first reviewed.
fn first_reviewed(plan: &std::path::Path) -> Option<u64> {
    let shown: Value =
        serde_json::from_str(&std::fs::read_to_string(plan.with_extension(SHOWN_EXTENSION)).ok()?)
            .ok()?;
    shown["reviewedAt"].as_u64()
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// The review the plan saved at `plan` followed up on, as saved beside it.
fn followed_review(plan: &std::path::Path) -> Option<plan::Previous> {
    let shown: Value =
        serde_json::from_str(&std::fs::read_to_string(plan.with_extension(SHOWN_EXTENSION)).ok()?)
            .ok()?;
    plan::Previous::from_json(&shown["followed"])
}

/// A CI rollup as the tour agent is told it.
fn ci_state(checks: crate::providers::CheckStatus) -> &'static str {
    use crate::providers::CheckStatus;
    match checks {
        CheckStatus::Passing => "passing",
        CheckStatus::Failing => "failing",
        CheckStatus::Pending => "pending",
        CheckStatus::Inconclusive => "inconclusive",
        CheckStatus::None => "none",
    }
}

/// Files a repository keeps its instructions for contributors and agents in.
const GUIDANCE_FILES: [&str; 3] = ["AGENTS.md", "CLAUDE.md", "CONTRIBUTING.md"];
/// Directories whose guidance is looked for, nearest the root first.
const MAX_GUIDANCE_DIRS: usize = 100;
const MAX_GUIDANCE_FILE_BYTES: usize = 16 * 1024;
const MAX_GUIDANCE_BYTES: usize = 48 * 1024;

/// The guidance files at `chapter`'s base in the repository root and in each
/// directory holding a changed file, root first, as (path, text). The base's,
/// not the head's, so a review cannot rewrite the rules it is judged by. A file is
/// cut short past a size, and past a total the files farthest from both the
/// root and the changes are left out, since the nearest file wins.
fn read_guidance(chapter: &Chapter) -> Result<Vec<(String, String)>> {
    let mut dirs = std::collections::BTreeSet::new();
    dirs.insert(String::new());
    for file in chapter.steps.iter().flat_map(|step| &step.files) {
        for path in [&file.old_path, &file.new_path] {
            let mut dir = path.as_str();
            while let Some((parent, _)) = dir.rsplit_once('/') {
                dirs.insert(format!("{parent}/"));
                dir = parent;
            }
        }
    }
    let mut dirs: Vec<String> = dirs.into_iter().collect();
    dirs.sort_by_key(|dir| dir.matches('/').count());
    dirs.truncate(MAX_GUIDANCE_DIRS);
    let paths: Vec<String> = dirs
        .iter()
        .flat_map(|dir| {
            GUIDANCE_FILES
                .iter()
                .map(move |name| format!("{dir}{name}"))
        })
        .collect();
    let specs: Vec<String> = paths
        .iter()
        .map(|path| format!("{}:{path}", chapter.base_sha))
        .collect();
    let texts = git::blob_texts(&specs, MAX_EMBEDDED_BYTES, MAX_EMBEDDED_BYTES)?;
    let found: Vec<(String, String)> = paths
        .into_iter()
        .zip(texts)
        .filter_map(|(path, text)| Some((path, text.filter(|text| !text.trim().is_empty())?)))
        .collect();
    Ok(within_guidance_budget(found))
}

/// `found` guidance, root first, cut down to its budget: the root's files are
/// kept first, then those nearest the changes.
fn within_guidance_budget(found: Vec<(String, String)>) -> Vec<(String, String)> {
    let (root, nested): (Vec<_>, Vec<_>) =
        found.into_iter().partition(|(path, _)| !path.contains('/'));
    let mut left = MAX_GUIDANCE_BYTES;
    let mut guidance = Vec::new();
    for (path, mut text) in root.into_iter().chain(nested.into_iter().rev()) {
        let room = MAX_GUIDANCE_FILE_BYTES.min(left);
        if room == 0 {
            break;
        }
        if text.len() > room {
            let mut at = room;
            while !text.is_char_boundary(at) {
                at -= 1;
            }
            text.truncate(at);
            text.push_str("\n[cut short]");
        }
        left = left.saturating_sub(text.len());
        guidance.push((path, text));
    }
    guidance
        .sort_by(|(a, _), (b, _)| (a.matches('/').count(), a).cmp(&(b.matches('/').count(), b)));
    guidance
}

/// Where a chapter's plan from `agent` is saved: per review, head, agent
/// command, and the instructions and plan version it was asked with, so any
/// of those changing asks again.
fn plan_cache(chapter: &Chapter, agent: &Agent, review: bool) -> Result<std::path::PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // The base too: a retargeted review keeps its head but changes its diff.
    (
        &chapter.base_sha,
        agent.display(),
        plan::VERSION,
        plan::instructions(review),
    )
        .hash(&mut hasher);
    let short = chapter.head_sha.get(..12).unwrap_or(&chapter.head_sha);
    Ok(
        git::git_common_path_absolute("stk/tours/plans")?.join(format!(
            "{}-{short}-{:016x}.json",
            chapter.review.id_value(),
            hasher.finish()
        )),
    )
}

/// The results of scoped threads, in order, re-raising a panic from any.
fn joined<T>(handles: Vec<thread::ScopedJoinHandle<'_, T>>) -> Vec<T> {
    handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
        .collect()
}

/// Each review's base tip and head, or None for a review that could not be
/// fetched. When the stack cannot be fetched at once, each review is fetched
/// on its own, so one missing ref costs only that review.
fn stack_tips(
    kind: ProviderKind,
    stack: &[ReviewRequest],
    current: usize,
    warnings: &mut Vec<String>,
) -> Result<Vec<Option<(String, String)>>> {
    match review_tips(kind, stack) {
        Ok(tips) => Ok(tips.into_iter().map(Some).collect()),
        Err(_) if stack.len() > 1 => {
            let mut tips = Vec::with_capacity(stack.len());
            for (index, review) in stack.iter().enumerate() {
                match review_tips(kind, std::slice::from_ref(review)) {
                    Ok(mut tip) => tips.push(tip.pop()),
                    // The asked-for review's own failure is the one to report.
                    Err(own) if index == current => return Err(own),
                    Err(error) => {
                        warnings.push(format!(
                            "could not fetch {}, so it is left out with the reviews stacked on it: {error:#}",
                            review.id
                        ));
                        tips.push(None);
                    }
                }
            }
            Ok(tips)
        }
        Err(error) => Err(error),
    }
}

/// Which reviews of a tree in depth-first order stay in the tour: from the
/// lowest review reachable below `current` through usable, linked reviews,
/// everything above it reachable the same way. `linked` says whether a
/// review really builds on its parent.
fn reachable_from_root(
    parents: &[Option<usize>],
    current: usize,
    usable: impl Fn(usize) -> bool,
    linked: impl Fn(usize) -> bool,
) -> Vec<bool> {
    let mut root = current;
    while let Some(parent) = parents[root].filter(|&parent| linked(root) && usable(parent)) {
        root = parent;
    }
    let mut reachable = vec![false; parents.len()];
    reachable[root] = true;
    for index in root + 1..parents.len() {
        reachable[index] = usable(index)
            && linked(index)
            && parents[index].is_some_and(|parent| reachable[parent]);
    }
    reachable
}

/// Which of `candidates`, reviews sharing the branch name `branch`, really is
/// that branch on the remote: the one whose head is the branch's tip. None
/// when none is, or when the remote cannot be asked, which is warned about;
/// either ends the stack there.
pub fn same_named_parent(
    kind: ProviderKind,
    branch: &str,
    candidates: &[&ReviewRequest],
    warnings: &RefCell<Vec<String>>,
) -> Option<usize> {
    settle_same_named(kind, branch, candidates, warnings, |references| {
        git::remote_commits(&settings::remote()?, references)
    })
}

fn settle_same_named(
    kind: ProviderKind,
    branch: &str,
    candidates: &[&ReviewRequest],
    warnings: &RefCell<Vec<String>>,
    remote_commits: impl FnOnce(&[String]) -> Result<Vec<Option<String>>>,
) -> Option<usize> {
    let mut references = vec![format!("refs/heads/{branch}")];
    for candidate in candidates {
        // The demo provider has no remote, and no forks to tell apart.
        references.push(head_ref(kind, candidate.id_value())?);
    }
    let commits = match remote_commits(&references) {
        Ok(commits) => commits,
        Err(error) => {
            warnings.borrow_mut().push(format!(
                "could not tell which review is {branch}, so the stack ends there: {error:#}"
            ));
            return None;
        }
    };
    let (Some(tip), heads) = commits.split_first()? else {
        return None;
    };
    heads.iter().position(|head| head.as_ref() == Some(tip))
}

/// Whether `review` has moved on from `head` on the remote, so comments
/// written against `head`'s diff could land on the wrong lines.
pub fn head_moved(kind: ProviderKind, review: &ReviewRequest, head: &str) -> Result<bool> {
    let Some(head_ref) = head_ref(kind, review.id_value()) else {
        return Ok(false);
    };
    let current = git::remote_commits(&settings::remote()?, &[head_ref])?;
    Ok(current.first().and_then(Option::as_deref) != Some(head))
}

/// The tip of each review's base branch and its head commit, fetched from the
/// remote in one round trip.
fn review_tips(kind: ProviderKind, stack: &[ReviewRequest]) -> Result<Vec<(String, String)>> {
    let mut references = Vec::with_capacity(stack.len() * 2);
    for review in stack {
        let Some(head_ref) = head_ref(kind, review.id_value()) else {
            // The demo provider's reviews live only in this repository.
            return stack
                .iter()
                .map(|review| {
                    Ok((
                        git::rev_parse(&review.base)?,
                        git::rev_parse(&review.branch)?,
                    ))
                })
                .collect();
        };
        references.push(format!("refs/heads/{}", review.base));
        references.push(head_ref);
    }
    let commits = git::fetch_commits(&settings::remote()?, &references)?;
    Ok(commits
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[base, head]| (base.clone(), head.clone()))
        .collect())
}

/// The ref a review's head is published under on the remote, or None for a
/// provider with no remote.
fn head_ref(kind: ProviderKind, number: &str) -> Option<String> {
    match kind {
        ProviderKind::GitHub | ProviderKind::Gitea => Some(format!("refs/pull/{number}/head")),
        ProviderKind::GitLab => Some(format!("refs/merge-requests/{number}/head")),
        ProviderKind::Demo => None,
    }
}

/// Read the whole text of both sides of every modified, renamed, or copied
/// file with hunks, for whole-file highlighting and context expansion. An added or
/// deleted file's hunk already holds every line, and generated and binary
/// files are not worth the space. Returns the bytes embedded, at most
/// `budget`.
fn embed_texts(steps: &mut [Step], base_sha: &str, head_sha: &str, budget: u64) -> Result<u64> {
    fn embeddable(step: &Step) -> impl Iterator<Item = &FileDiff> {
        step.files
            .iter()
            .filter(move |file| wants_text(step.kind, file))
    }
    let specs: Vec<String> = steps
        .iter()
        .flat_map(embeddable)
        .flat_map(|file| {
            [
                format!("{base_sha}:{}", file.old_path),
                format!("{head_sha}:{}", file.new_path),
            ]
        })
        .collect();
    let mut texts = git::blob_texts(&specs, MAX_EMBEDDED_BYTES, budget)?.into_iter();
    let mut embedded = 0;
    for step in steps.iter_mut() {
        let kind = step.kind;
        for file in step.files.iter_mut() {
            if wants_text(kind, file) {
                file.old_text = texts.next().flatten();
                file.new_text = texts.next().flatten();
                embedded += [&file.old_text, &file.new_text]
                    .into_iter()
                    .flatten()
                    .map(|text| text.len() as u64)
                    .sum::<u64>();
            }
        }
    }
    Ok(embedded)
}

fn wants_text(kind: steps::StepKind, file: &FileDiff) -> bool {
    kind != steps::StepKind::Generated
        && !file.binary
        // A pure rename or mode change has no hunk to expand around.
        && !file.hunks.is_empty()
        && !matches!(file.status, FileStatus::Added | FileStatus::Deleted)
}

/// Substitute each placeholder in turn, searching only the template text
/// after the previous one, so a value that happens to contain a later
/// placeholder - a diff of this template, say - is never substituted into.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    for (placeholder, value) in values {
        match rest.split_once(placeholder) {
            Some((before, after)) => {
                output.push_str(before);
                output.push_str(value);
                rest = after;
            }
            None => debug_assert!(false, "template has no {placeholder}"),
        }
    }
    output.push_str(rest);
    output
}

/// The highlight.js language for a path, or "" for plain text. Only names
/// in the bundled common build are useful; the page checks before using one.
fn language(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name {
        "Makefile" | "makefile" | "GNUmakefile" | "justfile" | "Justfile" => return "makefile",
        "Dockerfile" | "Containerfile" => return "bash",
        _ => {}
    }
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" | "tsx" => "typescript",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "m" | "mm" => "objectivec",
        "cs" => "csharp",
        "vb" => "vbnet",
        "rb" => "ruby",
        "php" => "php",
        "pl" | "pm" => "perl",
        "lua" => "lua",
        "r" => "r",
        "sh" | "bash" | "zsh" => "bash",
        "sql" => "sql",
        "graphql" | "gql" => "graphql",
        "css" => "css",
        "scss" => "scss",
        "less" => "less",
        "html" | "htm" | "xml" | "svg" | "vue" | "svelte" => "xml",
        "json" | "jsonc" => "json",
        "yaml" | "yml" => "yaml",
        "toml" | "ini" | "cfg" | "conf" => "ini",
        "md" | "markdown" | "mdx" => "markdown",
        "diff" | "patch" => "diff",
        "wat" | "wast" => "wasm",
        _ => "",
    }
}

fn file_data(file: &FileDiff) -> Value {
    json!({
        "path": file.path(),
        "oldPath": file.old_path,
        "status": file.status.as_str(),
        "binary": file.binary,
        "language": language(file.path()),
        "oldText": file.old_text,
        "newText": file.new_text,
        "additions": file.additions(),
        "deletions": file.deletions(),
        "hunks": file.hunks.iter().map(|hunk| json!({
            "oldStart": hunk.old_start,
            "oldLines": hunk.old_lines,
            "newStart": hunk.new_start,
            "newLines": hunk.new_lines,
            "section": hunk.section,
            // [kind, text] pairs keep a large diff's payload small.
            "lines": hunk.lines.iter().map(|line| json!([
                match line.kind {
                    LineKind::Context => " ",
                    LineKind::Added => "+",
                    LineKind::Deleted => "-",
                },
                line.text,
            ])).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// JSON that is safe inside a `<script>` element: no `</script>` or `<!--`
/// can end it early, and the line separators old engines reject are escaped.
fn script_json(value: &Value) -> String {
    value
        .to_string()
        .replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guidance_past_its_budget_keeps_the_root_and_the_nearest() {
        let big = "g".repeat(MAX_GUIDANCE_FILE_BYTES + 10);
        let found = [
            "AGENTS.md",
            "a/AGENTS.md",
            "a/b/AGENTS.md",
            "a/b/c/AGENTS.md",
        ]
        .map(|path| (path.to_owned(), big.clone()))
        .to_vec();
        let kept = within_guidance_budget(found);
        let paths: Vec<&str> = kept.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, ["AGENTS.md", "a/b/AGENTS.md", "a/b/c/AGENTS.md"]);
        assert!(kept.iter().all(|(_, text)| text.ends_with("[cut short]")));
    }

    #[test]
    fn reachable_from_root_drops_what_hangs_from_a_broken_review() {
        //    0
        //   / \
        //  1   3
        //  |   |
        //  2   4
        let parents = [None, Some(0), Some(1), Some(0), Some(3)];
        let all = reachable_from_root(&parents, 2, |_| true, |_| true);
        assert_eq!(all, [true; 5]);
        // A broken branch takes the reviews above it with it.
        let broken = reachable_from_root(&parents, 2, |index| index != 3, |_| true);
        assert_eq!(broken, [true, true, true, false, false]);
        // A broken review below the current one ends the stack there.
        let below = reachable_from_root(&parents, 2, |index| index != 0, |_| true);
        assert_eq!(below, [false, true, true, false, false]);
        // A current review that only shares its base's name stands alone
        // with what is above it, as does one whose parent is unlinked.
        let alone = reachable_from_root(&parents, 3, |_| true, |index| index != 3);
        assert_eq!(alone, [false, false, false, true, true]);
        let cut = reachable_from_root(&parents, 2, |_| true, |index| index != 1);
        assert_eq!(cut, [false, true, true, false, false]);
    }

    fn github_review(id: u32, branch: &str) -> ReviewRequest {
        ReviewRequest {
            id: format!("#{id}"),
            branch: branch.to_owned(),
            base: "main".to_owned(),
            state: crate::providers::ReviewState::Open,
            url: String::new(),
            title: String::new(),
            draft: false,
        }
    }

    #[test]
    fn same_named_reviews_are_settled_by_the_remote_tip() {
        let (fork, real) = (github_review(30, "a"), github_review(1, "a"));
        let warnings = RefCell::new(Vec::new());
        let chosen = settle_same_named(
            ProviderKind::GitHub,
            "a",
            &[&fork, &real],
            &warnings,
            |refs| {
                assert_eq!(
                    refs,
                    ["refs/heads/a", "refs/pull/30/head", "refs/pull/1/head"]
                );
                Ok(vec![
                    Some("tip".into()),
                    Some("other".into()),
                    Some("tip".into()),
                ])
            },
        );
        assert_eq!(chosen, Some(1));
        assert!(warnings.borrow().is_empty());
    }

    #[test]
    fn an_unreachable_remote_ends_the_stack_with_a_warning() {
        let (fork, real) = (github_review(30, "a"), github_review(1, "a"));
        let warnings = RefCell::new(Vec::new());
        let chosen = settle_same_named(
            ProviderKind::GitHub,
            "a",
            &[&fork, &real],
            &warnings,
            |_| bail!("connection refused"),
        );
        assert_eq!(chosen, None);
        assert_eq!(
            *warnings.borrow(),
            ["could not tell which review is a, so the stack ends there: connection refused"]
        );
    }

    #[test]
    fn script_json_cannot_close_the_script_element() {
        let json = script_json(&json!({ "text": "</script><!-- \u{2028}" }));
        assert!(!json.contains('<'), "{json}");
        assert!(json.contains("\\u003c/script>"));
        assert!(json.contains("\\u2028"));
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["text"], "</script><!-- \u{2028}");
    }

    #[test]
    fn template_carries_the_placeholders_in_fill_order() {
        let positions: Vec<usize> = [
            TITLE_PLACEHOLDER,
            LICENSE_PLACEHOLDER,
            HIGHLIGHT_PLACEHOLDER,
            DATA_PLACEHOLDER,
        ]
        .iter()
        .map(|placeholder| TEMPLATE.find(placeholder).expect(placeholder))
        .collect();
        assert!(positions.is_sorted(), "{positions:?}");
    }

    #[test]
    fn fill_never_substitutes_into_an_earlier_value() {
        let filled = fill(
            "<a>[1]</a><b>[2]</b>",
            &[("[1]", "has [2]"), ("[2]", "two")],
        );
        assert_eq!(filled, "<a>has [2]</a><b>two</b>");
    }

    #[test]
    fn vendored_highlighter_is_safe_to_inline() {
        // Either sequence would end the surrounding element early.
        assert!(!HIGHLIGHT_JS.to_ascii_lowercase().contains("</script"));
        assert!(!HIGHLIGHT_LICENSE.contains("--"));
    }

    #[test]
    fn language_maps_extensions_and_names() {
        assert_eq!(language("src/main.rs"), "rust");
        assert_eq!(language("web/App.TSX"), "typescript");
        assert_eq!(language("Makefile"), "makefile");
        assert_eq!(language("config/app.yml"), "yaml");
        assert_eq!(language("LICENSE"), "");
        assert_eq!(language("archive.tar.gz"), "");
    }
}
