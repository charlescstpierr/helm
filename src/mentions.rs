#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MentionTarget {
    Claude,
    Codex,
    Moi,
}

impl MentionTarget {
    pub const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Moi];

    pub fn slug(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Moi => "moi",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|target| target.slug().eq_ignore_ascii_case(name))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MentionSpan {
    pub start: usize,
    pub end: usize,
    pub target: MentionTarget,
}

fn continues_a_word(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '%' | '+' | '-' | '@')
}

fn in_a_name(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-')
}

pub fn find_mentions(body: &str) -> Vec<MentionSpan> {
    let mut spans = Vec::new();
    for (at, _) in body.match_indices('@') {
        if body[..at].chars().next_back().is_some_and(continues_a_word) {
            continue;
        }
        let name_start = at + 1;
        let rest = &body[name_start..];
        let name_len = rest.find(|c: char| !in_a_name(c)).unwrap_or(rest.len());
        let Some(target) = MentionTarget::from_name(&rest[..name_len]) else {
            continue;
        };
        let mut after = rest[name_len..].chars();
        let looks_like_domain_or_email = match after.next() {
            Some('@') => true,
            Some('.') => after.next().is_some_and(char::is_alphanumeric),
            _ => false,
        };
        if looks_like_domain_or_email {
            continue;
        }
        spans.push(MentionSpan {
            start: at,
            end: name_start + name_len,
            target,
        });
    }
    spans
}

pub fn mentioned_targets(body: &str) -> Vec<MentionTarget> {
    let mut targets = Vec::new();
    for span in find_mentions(body) {
        if !targets.contains(&span.target) {
            targets.push(span.target);
        }
    }
    targets
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment<'a> {
    Text(&'a str),
    Mention {
        text: &'a str,
        target: MentionTarget,
    },
}

pub fn segments(body: &str) -> Vec<Segment<'_>> {
    let mut segments = Vec::new();
    let mut cursor = 0;
    for span in find_mentions(body) {
        if span.start > cursor {
            segments.push(Segment::Text(&body[cursor..span.start]));
        }
        segments.push(Segment::Mention {
            text: &body[span.start..span.end],
            target: span.target,
        });
        cursor = span.end;
    }
    if cursor < body.len() {
        segments.push(Segment::Text(&body[cursor..]));
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use MentionTarget::{Claude, Codex, Moi};

    fn targets(body: &str) -> Vec<MentionTarget> {
        find_mentions(body).iter().map(|s| s.target).collect()
    }

    #[test]
    fn recognises_each_known_target() {
        assert_eq!(targets("@claude please look"), [Claude]);
        assert_eq!(targets("over to @codex"), [Codex]);
        assert_eq!(targets("blocked, waiting on @moi"), [Moi]);
    }

    #[test]
    fn recognises_a_mention_next_to_punctuation_and_line_breaks() {
        for body in [
            "Thanks @codex.",
            "Thanks @codex,",
            "(@codex)",
            "\"@codex\"",
            "ok?\n@codex",
            "@codex: done",
            "@codex's turn",
            "@CODEX",
            "@Codex",
        ] {
            assert_eq!(targets(body), [Codex], "{body:?}");
        }
    }

    #[test]
    fn ignores_what_is_not_a_known_target() {
        for body in [
            "@param name",
            "@someone",
            "@codexx",
            "@codex-bot",
            "@codex_",
            "@codéx",
            "@",
            "@ codex",
            "",
        ] {
            assert_eq!(targets(body), [], "{body:?}");
        }
    }

    #[test]
    fn ignores_email_addresses_and_domains() {
        for body in [
            "mail charles@codex.com",
            "a@codex",
            "john.doe+tag@moi.fr",
            "x_y@claude.ai",
            "é@codex",
            "see @codex.com",
            "run @codex.py",
            "@@codex",
            "@codex@example.com",
            "9@claude",
        ] {
            assert_eq!(targets(body), [], "{body:?}");
        }
    }

    #[test]
    fn a_trailing_full_stop_is_not_a_domain() {
        assert_eq!(
            targets("done, ping @codex. Then @claude.\n@moi."),
            [Codex, Claude, Moi]
        );
    }

    #[test]
    fn keeps_every_occurrence_but_lists_each_target_once() {
        let body = "@codex first, then @claude, then @codex again";
        assert_eq!(targets(body), [Codex, Claude, Codex]);
        assert_eq!(mentioned_targets(body), [Codex, Claude]);
    }

    #[test]
    fn spans_are_byte_ranges_of_the_original_text() {
        let body = "日本語 @codex été @Moi";
        let spans = find_mentions(body);
        let texts: Vec<&str> = spans.iter().map(|s| &body[s.start..s.end]).collect();
        assert_eq!(texts, ["@codex", "@Moi"]);
    }

    #[test]
    fn segments_reassemble_the_body_exactly() {
        for body in [
            "",
            "plain text",
            "@codex",
            "a @codex b @claude\n\nc a@codex.com @moi",
            "日本語 @codex été",
        ] {
            let joined: String = segments(body)
                .iter()
                .map(|s| match s {
                    Segment::Text(text) | Segment::Mention { text, .. } => *text,
                })
                .collect();
            assert_eq!(joined, body);
        }
    }

    #[test]
    fn segments_mark_only_real_mentions() {
        assert_eq!(
            segments("hi @codex, mail a@claude.ai"),
            [
                Segment::Text("hi "),
                Segment::Mention {
                    text: "@codex",
                    target: Codex
                },
                Segment::Text(", mail a@claude.ai"),
            ]
        );
    }

    #[test]
    fn from_name_round_trips_every_slug() {
        for target in MentionTarget::ALL {
            assert_eq!(MentionTarget::from_name(target.slug()), Some(target));
        }
        assert_eq!(MentionTarget::from_name("param"), None);
    }
}
