//! A tour plan an agent writes for one review (`stk.tourStyle = agentTour`):
//! an overview, the review's files in reading order as titled steps with a
//! short explanation each, and optional notes on lines of the diff. The plan
//! only reorders and explains what the diff already holds, and is checked
//! against it before use.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::diff::{FileDiff, LineKind};
use super::steps::{Step, StepKind, StepLabel};
use crate::providers::{ReviewRequest, Side};

/// The plan format's version; a plan saying another is not used.
pub const VERSION: u64 = 1;

/// How much of a review's diff the request carries: past this, files are
/// listed by name with their line counts.
const MAX_REQUEST_DIFF_BYTES: usize = 120_000;
/// Lines of one file's diff the request carries before cutting it short.
const MAX_FILE_LINES: usize = 400;
/// Notes kept from one plan.
const MAX_NOTES: usize = 20;

pub struct Plan {
    pub overview: String,
    pub steps: Vec<PlanStep>,
    pub notes: Vec<Note>,
    /// What the agent says it is, which git-stk cannot check.
    pub reported: Reported,
}

pub struct PlanStep {
    pub title: String,
    pub summary: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub path: String,
    pub side: Side,
    pub line: u32,
    pub body: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reported {
    pub provider: String,
    pub model: String,
    pub effort: String,
}

/// A plan as the tour uses it: its steps, laid over the review's files.
pub struct Applied {
    pub overview: String,
    pub steps: Vec<Step>,
    pub notes: Vec<Note>,
    pub reported: Reported,
    /// What the plan got wrong that was set right, for warnings.
    pub corrections: Vec<String>,
}

/// The plan in an agent's answer. Agents wrap JSON in prose or a code fence,
/// and some CLIs wrap their answer in a JSON envelope of their own (a
/// `result` string), so each is looked through.
pub fn parse(answer: &str) -> Result<Plan> {
    let value = find_json(answer).context("the agent's answer holds no JSON plan")?;
    let value = match value.get("result").and_then(Value::as_str) {
        Some(inner) if value.get("steps").is_none() => {
            find_json(inner).context("the agent's answer holds no JSON plan")?
        }
        _ => value,
    };
    if let Some(version) = value.get("version").and_then(Value::as_u64)
        && version != VERSION
    {
        bail!("the agent wrote a version {version} plan; git-stk reads version {VERSION}");
    }
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let steps = value
        .get("steps")
        .and_then(Value::as_array)
        .context("the plan has no steps")?
        .iter()
        .map(|step| PlanStep {
            title: text(step, "title"),
            summary: text(step, "summary"),
            files: step
                .get("files")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        })
        .collect();
    let notes = value
        .get("notes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|note| {
            Some(Note {
                path: text(note, "path"),
                side: if text(note, "side") == "old" {
                    Side::Old
                } else {
                    Side::New
                },
                line: u32::try_from(note.get("line")?.as_u64()?).ok()?,
                body: text(note, "body"),
            })
        })
        .collect();
    let agent = value.get("agent").cloned().unwrap_or_default();
    Ok(Plan {
        overview: text(&value, "overview"),
        steps,
        notes,
        reported: Reported {
            provider: text(&agent, "provider"),
            model: text(&agent, "model"),
            effort: text(&agent, "effort"),
        },
    })
}

/// The first JSON object in `text`: the whole of it, a ```json fence, or
/// the span from its first `{` to its last `}`.
fn find_json(text: &str) -> Option<Value> {
    let text = text.trim();
    let fenced = text
        .split_once("```json")
        .and_then(|(_, rest)| rest.split_once("```"))
        .map(|(inside, _)| inside);
    let braced = text
        .find('{')
        .zip(text.rfind('}'))
        .filter(|(start, end)| start < end)
        .map(|(start, end)| &text[start..=end]);
    [Some(text), fenced, braced]
        .into_iter()
        .flatten()
        .find_map(|candidate| {
            serde_json::from_str::<Value>(candidate.trim())
                .ok()
                .filter(Value::is_object)
        })
}

/// Whether `plan` can be laid over `steps`: it must place at least one of
/// their files.
pub fn check(plan: &Plan, steps: &[Step]) -> Result<()> {
    let places_any = plan.steps.iter().flat_map(|step| &step.files).any(|path| {
        steps
            .iter()
            .flat_map(|step| &step.files)
            .any(|file| file.path() == path || file.old_path == *path)
    });
    if !places_any {
        bail!("the plan places none of the review's files");
    }
    Ok(())
}

/// Lay a [`check`]ed `plan` over a review's `steps`: each file the plan
/// names, in its order, once; files it does not know are dropped, files it
/// leaves out go to a last step, and notes on lines the diff does not show
/// are dropped.
pub fn apply(plan: Plan, steps: Vec<Step>) -> Applied {
    let mut corrections = Vec::new();
    let mut pool: Vec<Option<(StepKind, FileDiff)>> = steps
        .into_iter()
        .flat_map(|step| {
            step.files
                .into_iter()
                .map(move |file| Some((step.kind, file)))
        })
        .collect();
    let lines_by_path: Vec<(String, HashSet<(Side, u32)>)> = pool
        .iter()
        .flatten()
        .map(|(_, file)| (file.path().to_owned(), diff_lines(file)))
        .collect();
    let take = |pool: &mut Vec<Option<(StepKind, FileDiff)>>, path: &str| {
        pool.iter_mut()
            .find(|entry| {
                entry
                    .as_ref()
                    .is_some_and(|(_, file)| file.path() == path || file.old_path == path)
            })
            .and_then(Option::take)
    };

    let mut planned = Vec::new();
    for (index, step) in plan.steps.into_iter().enumerate() {
        let mut files = Vec::new();
        for path in &step.files {
            match take(&mut pool, path) {
                Some(entry) => files.push(entry),
                None if lines_by_path.iter().any(|(known, _)| known == path) => {
                    corrections.push(format!("placed {path} twice"));
                }
                None => corrections.push(format!("named {path}, which the review does not change")),
            }
        }
        if files.is_empty() {
            continue;
        }
        let title = if step.title.is_empty() {
            format!("Step {}", index + 1)
        } else {
            step.title
        };
        planned.push(step_of(
            files,
            StepLabel {
                id: format!("plan-{}", index + 1),
                title,
                summary: step.summary,
            },
        ));
    }
    let rest: Vec<_> = pool.into_iter().flatten().collect();
    if !rest.is_empty() {
        let paths: Vec<&str> = rest.iter().map(|(_, file)| file.path()).collect();
        corrections.push(match paths.len() {
            0..=3 => format!("left out {}", paths.join(", ")),
            count => format!(
                "left out {} and {} more ({count} files)",
                paths[..3].join(", "),
                count - 3
            ),
        });
        planned.push(step_of(
            rest,
            StepLabel {
                id: "plan-rest".to_owned(),
                title: "Other changes".to_owned(),
                summary: String::new(),
            },
        ));
    }

    let mut notes = Vec::new();
    for note in plan.notes {
        let shown = lines_by_path
            .iter()
            .any(|(path, lines)| *path == note.path && lines.contains(&(note.side, note.line)));
        if !shown || note.body.is_empty() {
            corrections.push(format!(
                "noted {}:{}, which the diff does not show",
                note.path, note.line
            ));
        } else if notes.len() < MAX_NOTES {
            notes.push(note);
        }
    }
    Applied {
        overview: plan.overview,
        steps: planned,
        notes,
        reported: plan.reported,
        corrections,
    }
}

/// A planned step of `files`, collapsed when every file is generated.
fn step_of(files: Vec<(StepKind, FileDiff)>, label: StepLabel) -> Step {
    let kind = if files.iter().all(|(kind, _)| *kind == StepKind::Generated) {
        StepKind::Generated
    } else {
        StepKind::Source
    };
    Step {
        kind,
        files: files.into_iter().map(|(_, file)| file).collect(),
        label: Some(label),
    }
}

/// The lines a file's diff shows, by side.
fn diff_lines(file: &FileDiff) -> HashSet<(Side, u32)> {
    let mut lines = HashSet::new();
    for hunk in &file.hunks {
        let (mut old, mut new) = (hunk.old_start, hunk.new_start);
        for line in &hunk.lines {
            match line.kind {
                LineKind::Context => {
                    lines.insert((Side::Old, old));
                    lines.insert((Side::New, new));
                    old += 1;
                    new += 1;
                }
                LineKind::Deleted => {
                    lines.insert((Side::Old, old));
                    old += 1;
                }
                LineKind::Added => {
                    lines.insert((Side::New, new));
                    new += 1;
                }
            }
        }
    }
    lines
}

/// What a review offers its planner, and the plan it asks for back.
pub struct Request<'a> {
    pub review: &'a ReviewRequest,
    pub body: &'a str,
    pub commits: &'a [(String, String)],
    pub steps: &'a [Step],
    /// The reviews this one is stacked on, nearest last, by label.
    pub below: Vec<String>,
}

impl Request<'_> {
    /// The prompt for the agent: what to write, then the review as JSON.
    pub fn prompt(&self) -> String {
        let mut budget = MAX_REQUEST_DIFF_BYTES;
        let files: Vec<Value> = self
            .steps
            .iter()
            .flat_map(|step| step.files.iter().map(move |file| (step.kind, file)))
            .map(|(kind, file)| {
                let mut entry = json!({
                    "path": file.path(),
                    "status": file.status.as_str(),
                    "additions": file.additions(),
                    "deletions": file.deletions(),
                });
                if file.old_path != file.new_path {
                    entry["oldPath"] = file.old_path.clone().into();
                }
                if kind == StepKind::Generated {
                    entry["generated"] = true.into();
                } else if !file.binary {
                    let patch = patch_text(file);
                    if patch.len() <= budget {
                        budget -= patch.len();
                        entry["diff"] = patch.into();
                    } else {
                        entry["diffOmitted"] = true.into();
                    }
                }
                entry
            })
            .collect();
        let review = json!({
            "id": self.review.id,
            "title": self.review.title,
            "branch": self.review.branch,
            "base": self.review.base,
            "description": self.body,
            "commits": self.commits.iter().map(|(_, subject)| subject).collect::<Vec<_>>(),
            "stackedOn": self.below,
            "files": files,
        });
        format!("{INSTRUCTIONS}\n\nThe changeset:\n\n{review}\n")
    }
}

/// A file's hunks as unified diff text, cut short past [`MAX_FILE_LINES`].
fn patch_text(file: &FileDiff) -> String {
    let mut lines = Vec::new();
    'hunks: for hunk in &file.hunks {
        lines.push(format!(
            "@@ -{},{} +{},{} @@ {}",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines, hunk.section
        ));
        for line in &hunk.lines {
            if lines.len() >= MAX_FILE_LINES {
                lines.push("... (cut short)".to_owned());
                break 'hunks;
            }
            let sign = match line.kind {
                LineKind::Context => ' ',
                LineKind::Added => '+',
                LineKind::Deleted => '-',
            };
            lines.push(format!("{sign}{}", line.text));
        }
    }
    lines.join("\n")
}

// The agent reads "changeset" for what git-stk calls a review, so its
// instructions never ask for a review of a review.
pub const INSTRUCTIONS: &str = r#"You are planning a guided tour of one changeset, for a reader who will go through it step by step. Read the changeset below and answer with only a JSON object, no other text, shaped like this:

{
  "version": 1,
  "agent": {"provider": "who serves you", "model": "your model name", "effort": "your reasoning effort level, or empty if you do not know"},
  "overview": "Two to four sentences of Markdown: what the changeset does and why, and what to keep in mind while reading it.",
  "steps": [
    {"title": "A short title for this step", "summary": "One to three sentences of Markdown on what these files do in the changeset and what to look at.", "files": ["path/of/a/changed/file"]}
  ],
  "notes": [
    {"path": "path/of/a/changed/file", "side": "new", "line": 42, "body": "A short note on why this line matters."}
  ]
}

Rules:
- Order steps so each builds on what came before: the shape of the change first (types, interfaces, configuration), then what uses it, then tests, then documentation.
- Put every file the changeset touches in exactly one step, using the paths given. Group files that belong together; a step usually has one to five files.
- Generated files (marked "generated") go together in one last step.
- "stackedOn" lists the changesets this one builds on, oldest first; this tour covers only the changeset given.
- Notes are optional; add at most ten, only on lines that appear in the diff, with "side" "old" for a removed line and "new" otherwise.
- Describe what the code does; do not praise it or grade it.
- Fill "agent" with what you know about yourself, and leave a field empty rather than guess."#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tour::diff;

    fn steps_for(patch: &str) -> Vec<Step> {
        crate::tour::steps::group(diff::parse(patch), |path| path == "Cargo.lock")
    }

    const PATCH: &str = "diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,2 +1,2 @@
 fn a() {}
-fn b() {}
+fn c() {}
diff --git a/tests/a.rs b/tests/a.rs
--- a/tests/a.rs
+++ b/tests/a.rs
@@ -1 +1,2 @@
 #[test]
+fn t() {}
diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -1 +1 @@
-old
+new
";

    #[test]
    fn plans_are_found_in_prose_fences_and_envelopes() {
        let plan = r#"{"version":1,"overview":"O","steps":[{"title":"T","summary":"S","files":["src/a.rs"]}],"agent":{"provider":"P","model":"M","effort":"high"}}"#;
        for answer in [
            plan.to_owned(),
            format!("Here is the plan:\n```json\n{plan}\n```\nEnjoy."),
            format!("Sure! {plan} Done."),
            serde_json::json!({ "type": "result", "result": format!("```json\n{plan}\n```") })
                .to_string(),
        ] {
            let parsed = parse(&answer).unwrap_or_else(|error| panic!("{error}: {answer}"));
            assert_eq!(parsed.steps[0].files, ["src/a.rs"]);
            assert_eq!(
                parsed.reported,
                Reported {
                    provider: "P".into(),
                    model: "M".into(),
                    effort: "high".into()
                }
            );
        }
        assert!(parse("no plan here").is_err());
        assert!(parse(r#"{"version":2,"steps":[]}"#).is_err());
    }

    #[test]
    fn a_plan_orders_the_files_and_its_mistakes_are_set_right() {
        let plan = parse(
            r#"{"overview":"O","steps":[
                {"title":"Tests first","summary":"S","files":["tests/a.rs","nope.rs"]},
                {"title":"","summary":"","files":["tests/a.rs"]},
                {"title":"Code","summary":"S2","files":["src/a.rs"]}],
              "notes":[
                {"path":"src/a.rs","side":"new","line":2,"body":"Here"},
                {"path":"src/a.rs","side":"old","line":2,"body":"Gone"},
                {"path":"src/a.rs","side":"new","line":9,"body":"Nowhere"}]}"#,
        )
        .unwrap();
        let steps = steps_for(PATCH);
        check(&plan, &steps).unwrap();
        let applied = apply(plan, steps);
        let shape: Vec<(String, Vec<&str>, StepKind)> = applied
            .steps
            .iter()
            .map(|step| {
                (
                    step.label.as_ref().unwrap().title.clone(),
                    step.files.iter().map(FileDiff::path).collect(),
                    step.kind,
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                (
                    "Tests first".to_owned(),
                    vec!["tests/a.rs"],
                    StepKind::Source
                ),
                ("Code".to_owned(), vec!["src/a.rs"], StepKind::Source),
                (
                    "Other changes".to_owned(),
                    vec!["Cargo.lock"],
                    StepKind::Generated
                ),
            ]
        );
        assert_eq!(applied.notes.len(), 2, "lines the diff shows, either side");
        assert_eq!(
            applied.corrections,
            [
                "named nope.rs, which the review does not change",
                "placed tests/a.rs twice",
                "left out Cargo.lock",
                "noted src/a.rs:9, which the diff does not show",
            ]
        );
    }

    #[test]
    fn a_long_list_of_files_left_out_is_cut_short() {
        let plan = parse(r#"{"steps":[{"title":"T","files":["src/a.rs"]}]}"#).unwrap();
        let mut steps = steps_for(PATCH);
        let more = steps_for(
            &PATCH
                .replace("tests/a.rs", "tests/b.rs")
                .replace("Cargo.lock", "c.txt"),
        );
        steps.extend(
            more.into_iter()
                .filter(|step| step.files.iter().all(|file| file.path() != "src/a.rs")),
        );
        let applied = apply(plan, steps);
        assert_eq!(
            applied.corrections,
            ["left out tests/a.rs, Cargo.lock, tests/b.rs and 1 more (4 files)"]
        );
    }

    #[test]
    fn a_plan_that_places_nothing_is_not_used() {
        let plan = parse(r#"{"steps":[{"title":"T","files":["elsewhere.rs"]}]}"#).unwrap();
        assert!(check(&plan, &steps_for(PATCH)).is_err());
    }

    #[test]
    fn the_request_carries_the_diff_but_not_generated_files() {
        let steps = steps_for(PATCH);
        let review = ReviewRequest {
            id: "#7".into(),
            branch: "b".into(),
            base: "main".into(),
            state: crate::providers::ReviewState::Open,
            url: String::new(),
            title: "Rename b".into(),
            draft: false,
        };
        let prompt = Request {
            review: &review,
            body: "Why",
            commits: &[("abc".into(), "rename".into())],
            steps: &steps,
            below: vec!["Base work (#6)".into()],
        }
        .prompt();
        assert!(prompt.starts_with(INSTRUCTIONS));
        let review: Value =
            serde_json::from_str(prompt.rsplit_once("The changeset:").unwrap().1.trim()).unwrap();
        assert_eq!(review["stackedOn"][0], "Base work (#6)");
        let files = review["files"].as_array().unwrap();
        let lock = files
            .iter()
            .find(|file| file["path"] == "Cargo.lock")
            .unwrap();
        assert_eq!(lock["generated"], true);
        assert!(lock.get("diff").is_none());
        let source = files
            .iter()
            .find(|file| file["path"] == "src/a.rs")
            .unwrap();
        assert!(source["diff"].as_str().unwrap().contains("+fn c() {}"));
    }
}
