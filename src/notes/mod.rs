//! The managed blocks in review descriptions: the user's description, the
//! issue-closing link, and the stack-overview ledger ([`ledger`]), all
//! built on marker-delimited [`sections`].

use anyhow::Result;

use crate::providers::{ProviderKind, ReviewProvider, ReviewState};
use crate::settings;

mod ledger;
mod sections;
mod template;

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

/// Prepare each freshly created review's body before the managed sections go
/// in. Create-only - existing reviews keep whatever body they have.
///
/// With a repo PR/MR template (and `stk.usePrTemplate` on), the body is seeded
/// from it: the `--desc` branch (named by `desc_branch`) keeps the template
/// freeform above a seam so the description reads as a distinct block below,
/// while every other branch wraps the template in the managed description block
/// as the opening prose. Without a template the only cleanup is on the `--desc`
/// branch, whose body `create_review` seeded with the commit subject when the
/// commit had no body: that echo is dropped so the description does not sit
/// beneath a redundant copy of the title. A branch with neither a template nor
/// a description keeps its subject placeholder untouched. The template's source
/// is always the commit body, never the subject echo.
pub fn seed_template_notes(
    review_provider: &dyn ReviewProvider,
    kind: ProviderKind,
    created: &[String],
    desc_branch: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    if created.is_empty() {
        return Ok(());
    }
    let template = if settings::use_pr_template()? {
        template::discover(kind)?
    } else {
        None
    };

    for branch in created {
        let is_desc = desc_branch == Some(branch.as_str());
        // A branch with no template to seed and no description that would
        // strand the subject echo needs no change - leave create_review's body,
        // and make no provider call for it.
        if template.is_none() && !is_desc {
            continue;
        }
        if dry_run {
            anstream::println!("would seed the review body for {branch}");
            continue;
        }

        let Some(review) = review_provider.review_for_branch(branch)? else {
            anstream::println!("skipped body seed: no review found for {branch}");
            continue;
        };
        if review.branch != *branch {
            continue;
        }

        let body = review_provider.review_body(&review)?;
        let commit_body = crate::git::commit_body(branch)?;
        let prose = commit_body.trim();
        let (updated, seeded_template) = match (&template, is_desc) {
            // Template + description: template stays freeform above the seam.
            (Some(template), true) => (body_with_template(prose, template), true),
            // Template, no description: wrap it in the managed block.
            (Some(template), false) => (body_template_as_description(template, prose), true),
            // No template, description coming: drop create_review's subject echo,
            // keeping a real commit body if the commit had one.
            (None, true) => (prose.to_owned(), false),
            (None, false) => continue,
        };
        if updated == body {
            continue;
        }

        review_provider.update_review_body(&review, &updated)?;
        if seeded_template {
            anstream::println!("seeded the PR template into {}", review.id);
        } else {
            anstream::println!("dropped the commit subject from {}", review.id);
        }
    }

    Ok(())
}

/// Wrap the template (and any commit body) in the managed description block,
/// for a branch that gets no `--desc`. Keeping it inside the block - rather than
/// freeform above a seam - lets it read as the opening prose and keeps git-stk's
/// managed sections contiguous.
fn body_template_as_description(template: &str, prose: &str) -> String {
    let content = if prose.is_empty() {
        template.to_owned()
    } else {
        format!("{template}\n\n{prose}")
    };
    body_with_description_note("", &content)
}

/// Seed the `--desc` branch's body: the template, with the commit body beneath
/// it when the commit has one, above a horizontal-rule seam that separates it
/// from the managed description block `--desc` writes below. Always seamed - a
/// description always follows - and the caller's `updated == body` check is what
/// makes a re-seed a no-op, not any guard here.
fn body_with_template(prose: &str, template: &str) -> String {
    let freeform = if prose.trim().is_empty() {
        template.to_owned()
    } else {
        format!("{template}\n\n{}", prose.trim_start())
    };
    format!("{freeform}\n\n---")
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
    fn body_with_template_fills_an_empty_body() {
        assert_eq!(body_with_template("", "## Summary"), "## Summary\n\n---");
        assert_eq!(
            body_with_template("   \n", "## Summary"),
            "## Summary\n\n---"
        );
    }

    #[test]
    fn body_with_template_prepends_above_the_commit_body() {
        assert_eq!(
            body_with_template("Commit body.", "## Summary"),
            "## Summary\n\nCommit body.\n\n---"
        );
    }

    #[test]
    fn body_with_template_always_seams_even_when_the_commit_body_matches() {
        // A commit body that opens with the template text must not suppress the
        // seam: the `--desc` block always needs a rule above it.
        let seeded = body_with_template("## Summary\n\ndetails", "## Summary");
        assert_eq!(seeded, "## Summary\n\n## Summary\n\ndetails\n\n---");
    }

    #[test]
    fn body_template_as_description_wraps_the_template() {
        assert_eq!(
            body_template_as_description("## Summary", ""),
            "<!-- git-stk:description -->\n## Summary\n<!-- /git-stk:description -->"
        );
    }

    #[test]
    fn body_template_as_description_keeps_the_commit_body_below_the_template() {
        assert_eq!(
            body_template_as_description("## Summary", "Commit body."),
            "<!-- git-stk:description -->\n## Summary\n\nCommit body.\n<!-- /git-stk:description -->"
        );
    }

    #[test]
    fn seam_separates_the_template_from_the_managed_sections() {
        // Seed with a seam, then let the managed sections append below it -
        // they must land under the rule, not above or onto it.
        let seeded = body_with_template("", "## Summary\n\n- [ ] Tests");
        let with_desc = body_with_description_note(&seeded, "What and why.");
        let body = body_with_closes_note(&with_desc, "Closes #5");

        let template = body.find("- [ ] Tests").expect("template present");
        let rule = body.find("\n\n---\n\n").expect("seam rule present");
        let description = body.find("What and why.").expect("description below seam");
        let closes = body.find("Closes #5").expect("closes below seam");
        assert!(template < rule, "template sits above the seam");
        assert!(
            rule < description && rule < closes,
            "managed sections sit below the seam"
        );
        // Exactly one rule - the seam - not one per managed section.
        assert_eq!(body.matches("\n\n---\n\n").count(), 1, "{body}");
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
