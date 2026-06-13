//! An offline review provider for the guide and for experimenting:
//! `git config stk.provider demo`. Reviews live in a file under `.git`,
//! and merging performs a real squash onto the base branch, so the whole
//! merge loop works without a network or an account.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{MergeBlocker, ReviewProvider, ReviewRequest, ReviewState, command_output};
use crate::git;

pub(super) struct DemoProvider;

const STATE_FILE: &str = "stk-demo-reviews";

impl ReviewProvider for DemoProvider {
    fn review_for_branch(&self, branch: &str) -> Result<Option<ReviewRequest>> {
        let state = load()?;
        Ok(find(&state, branch, &["open", "merged"]).map(review_from))
    }

    fn review_for_branch_including_closed(&self, branch: &str) -> Result<Option<ReviewRequest>> {
        let state = load()?;
        Ok(find(&state, branch, &["open", "merged", "closed"]).map(review_from))
    }

    fn create_review(&self, branch: &str, base: &str, draft: bool) -> Result<String> {
        let mut state = load()?;
        if find(&state, branch, &["open"]).is_some() {
            bail!("demo review for {branch} already exists");
        }

        let id = state["next_id"].as_u64().unwrap_or(1);
        state["next_id"] = json!(id + 1);
        let title = command_output("git", &["log", "-1", "--format=%s", branch])?;
        state["reviews"]
            .as_array_mut()
            .context("demo state")?
            .push(json!({
                "id": id,
                "branch": branch,
                "base": base,
                "state": "open",
                "title": title,
                "body": "",
                "draft": draft,
            }));
        save(&state)?;
        Ok(format!("demo://review/{id}"))
    }

    fn update_review_base(&self, review: &ReviewRequest, base: &str) -> Result<String> {
        let mut state = load()?;
        with_review(&mut state, review, |entry| {
            entry["base"] = json!(base);
        })?;
        save(&state)?;
        Ok(String::new())
    }

    fn review_body(&self, review: &ReviewRequest) -> Result<String> {
        let state = load()?;
        let entry = by_id(&state, review)?;
        Ok(entry["body"].as_str().unwrap_or_default().to_owned())
    }

    fn update_review_body(&self, review: &ReviewRequest, body: &str) -> Result<String> {
        let mut state = load()?;
        with_review(&mut state, review, |entry| {
            entry["body"] = json!(body);
        })?;
        save(&state)?;
        Ok(String::new())
    }

    fn merge_review(&self, review: &ReviewRequest, _strategy: &str, _auto: bool) -> Result<String> {
        let mut state = load()?;
        let (branch, base, title) = {
            let entry = by_id(&state, review)?;
            if entry["state"] != json!("open") {
                bail!("demo review {} is not open", review.id);
            }
            (
                entry["branch"].as_str().unwrap_or_default().to_owned(),
                entry["base"].as_str().unwrap_or_default().to_owned(),
                entry["title"].as_str().unwrap_or_default().to_owned(),
            )
        };

        // A real squash, without touching the worktree: the branch's tree
        // becomes one new commit on top of the base.
        let tree = command_output("git", &["rev-parse", &format!("{branch}^{{tree}}")])?;
        let base_tip = command_output("git", &["rev-parse", &base])?;
        let message = format!("{title} ({})", review.id);
        let commit = command_output(
            "git",
            &["commit-tree", &tree, "-p", &base_tip, "-m", &message],
        )?;
        command_output(
            "git",
            &["update-ref", &format!("refs/heads/{base}"), commit.trim()],
        )?;

        with_review(&mut state, review, |entry| {
            entry["state"] = json!("merged");
        })?;
        save(&state)?;
        Ok(format!("squashed {branch} into {base}"))
    }

    fn merge_blocker(&self, _review: &ReviewRequest) -> Result<MergeBlocker> {
        // The demo merges locally and has no checks; nothing ever blocks.
        Ok(MergeBlocker::None)
    }

    fn wait_for_checks(&self, _review: &ReviewRequest) -> Result<bool> {
        // The demo has no CI; checks are always green.
        Ok(true)
    }

    fn open_reviews(&self) -> Result<Vec<ReviewRequest>> {
        let state = load()?;
        Ok(state["reviews"]
            .as_array()
            .map(|reviews| {
                reviews
                    .iter()
                    .filter(|review| review["state"].as_str() == Some("open"))
                    .map(review_from)
                    .collect()
            })
            .unwrap_or_default())
    }

    fn mark_ready(&self, review: &ReviewRequest) -> Result<String> {
        let mut state = load()?;
        with_review(&mut state, review, |entry| {
            entry["draft"] = json!(false);
        })?;
        save(&state)?;
        Ok(String::new())
    }

    fn close_review(&self, review: &ReviewRequest, _delete_branch: bool) -> Result<String> {
        let mut state = load()?;
        with_review(&mut state, review, |entry| {
            entry["state"] = json!("closed");
        })?;
        save(&state)?;
        Ok(format!("closed {}", review.id))
    }

    fn open_review(&self, _review: &ReviewRequest) -> Result<String> {
        // The demo has no web page to open.
        Ok("demo reviews have no web page".to_owned())
    }
}

fn state_path() -> Result<PathBuf> {
    Ok(PathBuf::from(git::git_common_path(STATE_FILE)?))
}

fn load() -> Result<Value> {
    let path = state_path()?;
    match fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).context("failed to parse demo state"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(json!({ "next_id": 1, "reviews": [] }))
        }
        Err(error) => Err(error).context("failed to read demo state"),
    }
}

fn save(state: &Value) -> Result<()> {
    let path = state_path()?;
    fs::write(&path, state.to_string())
        .with_context(|| format!("failed to write {}", path.display()))
}

/// The branch's review in the first matching state, by precedence.
fn find<'state>(state: &'state Value, branch: &str, states: &[&str]) -> Option<&'state Value> {
    let reviews = state["reviews"].as_array()?;
    for wanted in states {
        let found = reviews
            .iter()
            .find(|entry| entry["branch"] == json!(branch) && entry["state"] == json!(wanted));
        if found.is_some() {
            return found;
        }
    }
    None
}

fn by_id<'state>(state: &'state Value, review: &ReviewRequest) -> Result<&'state Value> {
    let id: u64 = review.id_value().parse().context("demo review id")?;
    state["reviews"]
        .as_array()
        .and_then(|reviews| reviews.iter().find(|entry| entry["id"] == json!(id)))
        .with_context(|| format!("no demo review {}", review.id))
}

fn with_review(
    state: &mut Value,
    review: &ReviewRequest,
    mutate: impl FnOnce(&mut Value),
) -> Result<()> {
    let id: u64 = review.id_value().parse().context("demo review id")?;
    let entry = state["reviews"]
        .as_array_mut()
        .and_then(|reviews| reviews.iter_mut().find(|entry| entry["id"] == json!(id)))
        .with_context(|| format!("no demo review {}", review.id))?;
    mutate(entry);
    Ok(())
}

fn review_from(entry: &Value) -> ReviewRequest {
    let id = entry["id"].as_u64().unwrap_or(0);
    let state = match entry["state"].as_str().unwrap_or("open") {
        "merged" => ReviewState::Merged,
        "closed" => ReviewState::Closed,
        _ => ReviewState::Open,
    };
    ReviewRequest {
        id: format!("#{id}"),
        branch: entry["branch"].as_str().unwrap_or_default().to_owned(),
        base: entry["base"].as_str().unwrap_or_default().to_owned(),
        state,
        url: format!("demo://review/{id}"),
        title: entry["title"].as_str().unwrap_or_default().to_owned(),
        draft: entry["draft"].as_bool().unwrap_or(false),
    }
}
