//! The stack a review belongs to, found from review bases alone: a review
//! whose base is another open review's branch is stacked on it. This reads
//! the same on every forge and needs no git-stk metadata, so it finds stacks
//! made by any tool or by hand. Names alone can mislead across forks, so
//! [`crate::tour`] also checks each link against the fetched commits.

use std::collections::HashSet;

use crate::providers::ReviewRequest;

/// A review and the reviews stacked with it, as a tree in depth-first order:
/// each review comes after its parent, and siblings in review-number order.
pub struct Stack {
    pub reviews: Vec<ReviewRequest>,
    /// The index in `reviews` of each review's parent; None for the root.
    pub parents: Vec<Option<usize>>,
    /// The index of the review asked for.
    pub current: usize,
}

impl Stack {
    pub fn single(review: ReviewRequest) -> Self {
        Self {
            reviews: vec![review],
            parents: vec![None],
            current: 0,
        }
    }
}

/// The stack `target` belongs to: the reviews below it down to the trunk,
/// and every review stacked on those, branches included. Where several
/// reviews share the branch name a review is based on, as a fork's can,
/// `pick` chooses the one that is really that branch, or none to end the
/// stack there. A review of a `long_lived` branch, such as a release review
/// from `develop` into `main`, is no part of the stacks built on that branch.
pub fn around(
    target: ReviewRequest,
    mut pool: Vec<ReviewRequest>,
    pick: &dyn Fn(&str, &[&ReviewRequest]) -> Option<usize>,
    long_lived: &dyn Fn(&str) -> bool,
) -> Stack {
    // Only a fork's review can have the same branch name as its base, and
    // it is no parent of the reviews on that base.
    pool.retain(|review| {
        review.branch != review.base && review.id != target.id && !long_lived(&review.branch)
    });
    pool.push(target);
    let target = pool.len() - 1;

    // The line down from the target, each review's parent after it.
    let mut line = vec![target];
    let mut seen = HashSet::from([pool[target].branch.as_str()]);
    loop {
        let base = pool[line[line.len() - 1]].base.as_str();
        let candidates: Vec<usize> = (0..pool.len())
            .filter(|&index| pool[index].branch == base)
            .collect();
        let parent = match candidates.as_slice() {
            [] => break,
            [only] => *only,
            several => {
                let reviews: Vec<&ReviewRequest> = several.iter().map(|&i| &pool[i]).collect();
                match pick(base, &reviews) {
                    Some(chosen) if chosen < several.len() => several[chosen],
                    _ => break,
                }
            }
        };
        if !seen.insert(pool[parent].branch.as_str()) {
            break;
        }
        line.push(parent);
    }

    // Down the tree from the bottom review. The line's links are fixed, so a
    // same-named review elsewhere cannot claim a review on it.
    let on_line = |index: usize| line.contains(&index);
    let line_child = |index: usize| {
        line.iter()
            .position(|&review| review == index)
            .filter(|&position| position > 0)
            .map(|position| line[position - 1])
    };
    let root = line[line.len() - 1];
    let mut order = Vec::new();
    let mut parents = Vec::new();
    let mut visited = vec![false; pool.len()];
    let mut pending = vec![(root, None)];
    while let Some((index, parent)) = pending.pop() {
        if std::mem::replace(&mut visited[index], true) {
            continue;
        }
        let position = order.len();
        order.push(index);
        parents.push(parent);
        let mut children: Vec<usize> = if long_lived(&pool[index].branch) {
            Vec::new()
        } else {
            (0..pool.len())
                .filter(|&child| {
                    !visited[child] && !on_line(child) && pool[child].base == pool[index].branch
                })
                .chain(line_child(index))
                .collect()
        };
        children.sort_by_key(|&child| std::cmp::Reverse(review_number(&pool[child])));
        pending.extend(children.into_iter().map(|child| (child, Some(position))));
    }

    let current = order
        .iter()
        .position(|&index| index == target)
        .unwrap_or_default();
    let mut pool: Vec<Option<ReviewRequest>> = pool.into_iter().map(Some).collect();
    Stack {
        reviews: order
            .iter()
            .filter_map(|&index| pool[index].take())
            .collect(),
        parents,
        current,
    }
}

/// Whether `branch` is a long-lived branch - the repository's default, or
/// one conventionally kept for integration or release - rather than the
/// topic branch of one review.
pub fn long_lived(branch: &str, default_branch: Option<&str>) -> bool {
    default_branch == Some(branch)
        || matches!(
            branch,
            "main"
                | "master"
                | "trunk"
                | "develop"
                | "development"
                | "dev"
                | "next"
                | "staging"
                | "stable"
                | "production"
                | "prod"
        )
        || ["release/", "releases/", "stable/"]
            .iter()
            .any(|prefix| branch.starts_with(prefix))
}

/// A review's number for ordering siblings; ids without one sort first.
fn review_number(review: &ReviewRequest) -> u64 {
    review.id_value().parse().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ReviewState;

    fn review(id: u32, branch: &str, base: &str) -> ReviewRequest {
        ReviewRequest {
            id: format!("#{id}"),
            branch: branch.to_owned(),
            base: base.to_owned(),
            state: ReviewState::Open,
            url: String::new(),
            title: String::new(),
            draft: false,
        }
    }

    fn no_pick(_: &str, _: &[&ReviewRequest]) -> Option<usize> {
        None
    }

    fn no_trunk(_: &str) -> bool {
        false
    }

    /// The stack as `(id, parent id)` pairs in order, and the current id.
    fn shape(stack: &Stack) -> (Vec<(&str, Option<&str>)>, &str) {
        let pairs = stack
            .reviews
            .iter()
            .zip(&stack.parents)
            .map(|(review, parent)| {
                (
                    review.id.as_str(),
                    parent.map(|parent| stack.reviews[parent].id.as_str()),
                )
            })
            .collect();
        (pairs, stack.reviews[stack.current].id.as_str())
    }

    #[test]
    fn follows_bases_down_and_children_up() {
        let open = vec![
            review(3, "c", "b"),
            review(1, "a", "main"),
            review(9, "other", "main"),
            review(2, "b", "a"),
        ];
        let stack = around(review(2, "b", "a"), open, &no_pick, &no_trunk);
        assert_eq!(
            shape(&stack),
            (
                vec![("#1", None), ("#2", Some("#1")), ("#3", Some("#2"))],
                "#2"
            )
        );
    }

    #[test]
    fn a_review_on_trunk_with_nothing_above_stands_alone() {
        let open = vec![review(1, "a", "main"), review(2, "b", "main")];
        let stack = around(review(1, "a", "main"), open, &no_pick, &no_trunk);
        assert_eq!(shape(&stack), (vec![("#1", None)], "#1"));
    }

    #[test]
    fn branches_of_the_stack_are_toured_depth_first() {
        let open = || {
            vec![
                review(4, "d", "c"),
                review(3, "c", "a"),
                review(1, "a", "main"),
                review(2, "b", "a"),
            ]
        };
        let expected = vec![
            ("#1", None),
            ("#2", Some("#1")),
            ("#3", Some("#1")),
            ("#4", Some("#3")),
        ];
        // The same tree from any review in it.
        for target in [review(1, "a", "main"), review(4, "d", "c")] {
            let id = target.id.clone();
            let stack = around(target, open(), &no_pick, &no_trunk);
            assert_eq!(shape(&stack), (expected.clone(), id.as_str()));
        }
    }

    #[test]
    fn a_closed_target_still_finds_its_open_neighbors() {
        let open = vec![review(1, "a", "main"), review(3, "c", "b")];
        let stack = around(review(2, "b", "a"), open, &no_pick, &no_trunk);
        assert_eq!(
            shape(&stack),
            (
                vec![("#1", None), ("#2", Some("#1")), ("#3", Some("#2"))],
                "#2"
            )
        );
    }

    #[test]
    fn a_fork_review_from_its_own_trunk_is_no_parent() {
        let open = vec![review(7, "main", "main"), review(1, "a", "main")];
        let stack = around(review(1, "a", "main"), open, &no_pick, &no_trunk);
        assert_eq!(shape(&stack), (vec![("#1", None)], "#1"));
    }

    #[test]
    fn same_named_parents_are_settled_by_pick() {
        let open = || {
            vec![
                review(30, "a", "main"),
                review(1, "a", "main"),
                review(2, "b", "a"),
            ]
        };
        let pick_second = |base: &str, candidates: &[&ReviewRequest]| {
            assert_eq!(base, "a");
            let ids: Vec<&str> = candidates.iter().map(|review| review.id.as_str()).collect();
            assert_eq!(ids, ["#30", "#1"]);
            Some(1)
        };
        let stack = around(review(2, "b", "a"), open(), &pick_second, &no_trunk);
        assert_eq!(
            shape(&stack),
            (vec![("#1", None), ("#2", Some("#1"))], "#2")
        );
        let stack = around(review(2, "b", "a"), open(), &no_pick, &no_trunk);
        assert_eq!(shape(&stack), (vec![("#2", None)], "#2"));
    }

    #[test]
    fn a_review_of_a_long_lived_branch_is_no_stack_parent() {
        let open = || {
            vec![
                review(5, "develop", "main"),
                review(1, "feature-a", "develop"),
                review(2, "feature-b", "develop"),
                review(3, "feature-c", "feature-a"),
            ]
        };
        let develop = |branch: &str| long_lived(branch, Some("main"));
        let stack = around(
            review(1, "feature-a", "develop"),
            open(),
            &no_pick,
            &develop,
        );
        assert_eq!(
            shape(&stack),
            (vec![("#1", None), ("#3", Some("#1"))], "#1")
        );
        // Nor are the reviews into it a stack on the release review itself.
        let stack = around(review(5, "develop", "main"), open(), &no_pick, &develop);
        assert_eq!(shape(&stack), (vec![("#5", None)], "#5"));
    }

    #[test]
    fn long_lived_branches_are_told_from_topic_branches() {
        for branch in ["main", "develop", "release/1.2", "stable", "trunk-next"] {
            assert!(
                long_lived(branch, Some("trunk-next")),
                "{branch} is long-lived"
            );
        }
        for branch in ["feature/x", "fix-develop", "hotfix/1.2.1", "part-1"] {
            assert!(!long_lived(branch, Some("main")), "{branch} is a topic");
        }
    }

    #[test]
    fn a_cycle_of_bases_ends() {
        let open = vec![review(1, "a", "b"), review(2, "b", "a")];
        let stack = around(review(1, "a", "b"), open, &no_pick, &no_trunk);
        assert_eq!(
            shape(&stack),
            (vec![("#2", None), ("#1", Some("#2"))], "#1")
        );
    }
}
