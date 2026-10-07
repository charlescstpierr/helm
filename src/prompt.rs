//! The instruction an agent receives for a card: what the card says, what was said about it,
//! and how to hand the work back. Pure text in, text out.

use crate::store::{AuthorKind, Card, Comment};

/// `key` is the card's identifier (`HELM-12`) and `branch` the branch its worktree is on.
pub fn build(key: &str, branch: &str, card: &Card, comments: &[Comment]) -> String {
    let mut prompt = format!("You are working on card {key}: {}\n\n", card.title);
    if card.description.trim().is_empty() {
        prompt.push_str("The card has no description.\n");
    } else {
        prompt.push_str(&card.description);
        prompt.push('\n');
    }
    if !comments.is_empty() {
        prompt.push_str("\nThe discussion on the card so far, oldest first:\n");
        for comment in comments {
            let who = match comment.author.kind {
                AuthorKind::Human => "human",
                AuthorKind::Agent => "agent",
                AuthorKind::System => "helm",
            };
            prompt.push_str(&format!(
                "\n[{} ({who}), {}]\n{}\n",
                comment.author.name,
                comment.created_display(),
                comment.body
            ));
        }
    }
    prompt.push_str(&format!(
        "\nHow to work:\n\
         - You are in a dedicated git worktree on branch `{branch}`. Stay in this directory and on this branch.\n\
         - Commit your work to the current branch with clear commit messages.\n\
         - Do not push. Helm pushes the branch to origin when you finish.\n\
         - If the task is unclear or cannot be done, say why in your final message and commit nothing.\n"
    ));
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Author, Card};

    fn card(title: &str, description: &str) -> Card {
        Card {
            id: 1,
            column_id: 2,
            number: 7,
            title: title.to_owned(),
            description: description.to_owned(),
            priority: 0,
            labels: Vec::new(),
            comment_count: 0,
            agent: None,
            model: None,
        }
    }

    fn comment(kind: AuthorKind, name: &str, body: &str) -> Comment {
        Comment {
            id: 1,
            author: Author {
                kind,
                name: name.to_owned(),
            },
            body: body.to_owned(),
            created_at: 0,
        }
    }

    #[test]
    fn the_prompt_carries_title_description_thread_and_the_hand_back_rules() {
        let prompt = build(
            "HELM-7",
            "helm/HELM-7",
            &card("Add a HELLO file", "One line, please."),
            &[
                comment(AuthorKind::Human, "moi", "also mention Helm"),
                comment(AuthorKind::System, "helm", "Run 3 failed: boom"),
            ],
        );
        assert!(prompt.starts_with(
            "You are working on card HELM-7: Add a HELLO file\n\nOne line, please.\n"
        ));
        let human = prompt
            .find("[moi (human), 1970-01-01 00:00 UTC]\nalso mention Helm")
            .unwrap();
        let system = prompt
            .find("[helm (helm), 1970-01-01 00:00 UTC]\nRun 3 failed: boom")
            .unwrap();
        assert!(human < system, "oldest first");
        assert!(prompt.contains("branch `helm/HELM-7`"));
        assert!(prompt.contains("Do not push"));
    }

    #[test]
    fn a_card_without_description_or_comments_still_gets_a_complete_prompt() {
        let prompt = build("HELM-1", "helm/HELM-1", &card("Just a title", "  "), &[]);
        assert!(prompt.contains("The card has no description."));
        assert!(!prompt.contains("discussion"));
        assert!(prompt.contains("Commit your work"));
    }
}
