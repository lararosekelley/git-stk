//! The managed blocks in review descriptions: the user's description, the
//! issue-closing link, and the stack-overview ledger ([`ledger`]), all
//! built on marker-delimited [`sections`].

use anyhow::Result;

use crate::providers::{ReviewProvider, ReviewState};

mod ledger;
mod sections;

pub use ledger::update_stack_notes;

use sections::{body_with_section_before, marker_start, strip_sections};

const STACK_SECTION: &str = "stack";
const CLOSES_SECTION: &str = "closes";
const DESCRIPTION_SECTION: &str = "description";

/// Add a `Closes #N` line to each branch's review when the branch name
/// references an issue (e.g. `123-fix-thing`, `fix/issue-123`), so the
/// platform closes the issue when the review merges. Branches without an
/// issue reference are passed over silently.
pub fn update_closes_notes(
    review_provider: &dyn ReviewProvider,
    branches: &[String],
    dry_run: bool,
) -> Result<()> {
    for branch in branches {
        let Some(issue) = issue_number_from_branch(branch) else {
            continue;
        };

        let Some(review) = review_provider.review_for_branch(branch)? else {
            // On a dry run the review was likely never created; for real the
            // submit just failed to produce one, which deserves a mention.
            if dry_run {
                anstream::println!("would link issue #{issue} in the review for {branch}");
            } else {
                anstream::println!("skipped issue link: no review found for {branch}");
            }
            continue;
        };

        if review.branch != *branch || review.state == ReviewState::Merged {
            continue;
        }

        if dry_run {
            anstream::println!("would link issue #{issue} in {}", review.id);
            continue;
        }

        let body = review_provider.review_body(&review)?;
        let updated = body_with_closes_note(&body, &format!("Closes #{issue}"));
        if updated == body {
            continue;
        }

        review_provider.update_review_body(&review, &updated)?;
        anstream::println!("linked issue #{issue} in {}", review.id);
    }

    Ok(())
}

/// Write (or, with an empty string, clear) the description block in the
/// branch's review body. Unlike the stack overview the block is sticky:
/// submits without `--desc` never touch it.
pub fn update_description_note(
    review_provider: &dyn ReviewProvider,
    branch: &str,
    description: &str,
    dry_run: bool,
) -> Result<()> {
    let verb = if description.is_empty() {
        "clear"
    } else {
        "set"
    };

    let Some(review) = review_provider.review_for_branch(branch)? else {
        if dry_run {
            anstream::println!("would {verb} the description on the review for {branch}");
        } else {
            anstream::println!("skipped description: no review found for {branch}");
        }
        return Ok(());
    };
    if review.branch != *branch {
        anstream::println!(
            "skipped description: review {} belongs to {}",
            review.id,
            review.branch
        );
        return Ok(());
    }

    if dry_run {
        anstream::println!("would {verb} the description in {}", review.id);
        return Ok(());
    }

    let body = review_provider.review_body(&review)?;
    let updated = if description.is_empty() {
        if !body.contains(&marker_start(DESCRIPTION_SECTION)) {
            return Ok(());
        }
        strip_sections(&body, DESCRIPTION_SECTION)
            .trim_end()
            .to_owned()
    } else {
        body_with_description_note(&body, description)
    };
    if updated == body {
        return Ok(());
    }

    review_provider.update_review_body(&review, &updated)?;
    anstream::println!(
        "{} description in {}",
        if description.is_empty() {
            "cleared"
        } else {
            "set"
        },
        review.id
    );
    Ok(())
}

/// The issue number a branch name refers to, if any. A path segment that
/// starts with the number (`123-fix-thing`, `fix/123-thing`, bare `123`) or
/// prefixes it with issue/issues (`issue-123`, `fix/issues-123-thing`)
/// counts; trailing numbers do not, to keep version-ish names from
/// closing unrelated issues.
fn issue_number_from_branch(branch: &str) -> Option<u64> {
    for segment in branch.split('/') {
        let lowered = segment.to_ascii_lowercase();
        let candidate = lowered
            .strip_prefix("issue-")
            .or_else(|| lowered.strip_prefix("issues-"))
            .unwrap_or(&lowered);

        let end = candidate
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(candidate.len());
        let (digits, rest) = candidate.split_at(end);
        if digits.is_empty() || !(rest.is_empty() || rest.starts_with('-')) {
            continue;
        }

        if let Ok(number) = digits.parse::<u64>()
            && number > 0
        {
            return Some(number);
        }
    }

    None
}

/// Splice the closes note in, keeping it above the stack overview so the
/// closing keyword reads as part of the description rather than the footer.
fn body_with_closes_note(body: &str, note: &str) -> String {
    body_with_section_before(body, CLOSES_SECTION, note, &[STACK_SECTION])
}

/// Splice the user's description in, above every managed section so it
/// reads as the opening of the body.
fn body_with_description_note(body: &str, description: &str) -> String {
    body_with_section_before(
        body,
        DESCRIPTION_SECTION,
        description,
        &[CLOSES_SECTION, STACK_SECTION],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_number_from_branch_reads_supported_shapes() {
        assert_eq!(issue_number_from_branch("123-fix-thing"), Some(123));
        assert_eq!(issue_number_from_branch("fix/123-thing"), Some(123));
        assert_eq!(issue_number_from_branch("fix/issue-123"), Some(123));
        assert_eq!(issue_number_from_branch("feat/issues-9-cleanup"), Some(9));
        assert_eq!(issue_number_from_branch("42"), Some(42));
    }

    #[test]
    fn issue_number_from_branch_rejects_lookalikes() {
        assert_eq!(issue_number_from_branch("feature/b"), None);
        assert_eq!(issue_number_from_branch("fix-thing-123"), None);
        assert_eq!(issue_number_from_branch("v2-migration"), None);
        assert_eq!(issue_number_from_branch("2024q1-cleanup"), None);
        assert_eq!(issue_number_from_branch("0-zero"), None);
        assert_eq!(issue_number_from_branch("upgrade-issue"), None);
    }

    #[test]
    fn body_with_closes_note_appends_without_a_stack_section() {
        let updated = body_with_closes_note("Description.", "Closes #5");
        assert_eq!(
            updated,
            "Description.\n\n<!-- git-stk:closes -->\nCloses #5\n<!-- /git-stk:closes -->"
        );
    }

    #[test]
    fn body_with_closes_note_lands_above_the_stack_section() {
        let body = "Description.\n\n<!-- git-stk:stack -->\nstack list\n<!-- /git-stk:stack -->";
        let updated = body_with_closes_note(body, "Closes #5");
        assert_eq!(
            updated,
            "Description.\n\n\
             <!-- git-stk:closes -->\nCloses #5\n<!-- /git-stk:closes -->\n\n\
             <!-- git-stk:stack -->\nstack list\n<!-- /git-stk:stack -->"
        );
    }

    #[test]
    fn body_with_closes_note_replaces_a_stale_note_in_place() {
        let body = "Intro.\n\n<!-- git-stk:closes -->\nCloses #4\n<!-- /git-stk:closes -->\n\n\
                    <!-- git-stk:stack -->\nstack list\n<!-- /git-stk:stack -->";
        let updated = body_with_closes_note(body, "Closes #5");
        assert_eq!(updated.matches("<!-- git-stk:closes -->").count(), 1);
        assert!(updated.contains("Closes #5"));
        assert!(!updated.contains("Closes #4"));
        let closes = updated.find("Closes #5").expect("closes note");
        let stack = updated.find("stack list").expect("stack note");
        assert!(
            closes < stack,
            "closes note should sit above the stack note"
        );
    }

    #[test]
    fn body_with_description_note_lands_above_every_managed_section() {
        let body = "Intro.\n\n\
                    <!-- git-stk:closes -->\nCloses #5\n<!-- /git-stk:closes -->\n\n\
                    <!-- git-stk:stack -->\nstack list\n<!-- /git-stk:stack -->";
        let updated = body_with_description_note(body, "Summary.");

        let intro = updated.find("Intro.").expect("intro");
        let description = updated.find("Summary.").expect("description");
        let closes = updated.find("Closes #5").expect("closes");
        let stack = updated.find("stack list").expect("stack");
        assert!(intro < description && description < closes && closes < stack);
        assert!(
            updated
                .contains("<!-- git-stk:description -->\nSummary.\n<!-- /git-stk:description -->")
        );
    }

    #[test]
    fn body_with_description_note_replaces_in_place() {
        let body = "<!-- git-stk:description -->\nOld.\n<!-- /git-stk:description -->\n\n\
                    <!-- git-stk:stack -->\nstack list\n<!-- /git-stk:stack -->";
        let updated = body_with_description_note(body, "New.");
        assert_eq!(updated.matches("<!-- git-stk:description -->").count(), 1);
        assert!(updated.contains("New."));
        assert!(!updated.contains("Old."));
        let description = updated.find("New.").expect("description");
        let stack = updated.find("stack list").expect("stack");
        assert!(description < stack);
    }
}
