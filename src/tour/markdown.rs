//! A review description rendered from Markdown to HTML that is safe to show
//! in the tour page: no raw HTML, no script links, and no remote images.

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};

pub fn to_html(markdown: &str) -> String {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM;
    let mut stripper = TagStripper::default();
    let events = Parser::new_ext(markdown, options).map(|event| {
        if !matches!(event, Event::Html(_) | Event::InlineHtml(_)) {
            // A tag never spans past its block, so an unclosed `<` ends here.
            stripper = TagStripper::default();
        }
        sanitize(event, &mut stripper)
    });
    let mut output = String::new();
    html::push_html(&mut output, events);
    output
}

fn sanitize<'a>(event: Event<'a>, stripper: &mut TagStripper) -> Event<'a> {
    match event {
        // Raw HTML would run in the page; keep only the text between its tags.
        Event::Html(markup) | Event::InlineHtml(markup) => {
            Event::Text(stripper.strip(&markup).into())
        }
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: safe_url(dest_url),
            title,
            id,
        }),
        // An image becomes a link to it, so opening a tour fetches nothing.
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: safe_url(dest_url),
            title,
            id,
        }),
        Event::End(TagEnd::Image) => Event::End(TagEnd::Link),
        other => other,
    }
}

/// `url` when it is a web, mail, or relative link; otherwise nothing, so a
/// `javascript:` or `data:` URL cannot be followed.
fn safe_url(url: CowStr<'_>) -> CowStr<'_> {
    let scheme = url
        .split_once(':')
        .map(|(scheme, _)| scheme)
        .filter(|scheme| !scheme.contains(['/', '?', '#']));
    match scheme.map(str::to_ascii_lowercase).as_deref() {
        None | Some("http" | "https" | "mailto") => url,
        Some(_) => CowStr::Borrowed(""),
    }
}

/// Removes tags and comments from raw HTML, keeping the text between them.
/// The parser hands block HTML over a line at a time, so a tag or comment
/// can span several calls.
#[derive(Default)]
struct TagStripper {
    state: StripState,
    /// The last characters seen inside a tag or comment, to spot `<!--` and `-->`.
    recent: String,
    /// The quote an attribute value inside a tag is open with, where `>` does
    /// not end the tag.
    quote: Option<char>,
}

#[derive(Default, PartialEq)]
enum StripState {
    #[default]
    Text,
    Tag,
    Comment,
}

impl TagStripper {
    fn strip(&mut self, markup: &str) -> String {
        let mut text = String::with_capacity(markup.len());
        let mut chars = markup.chars().peekable();
        while let Some(ch) = chars.next() {
            let opens_tag = ch == '<'
                && chars
                    .peek()
                    .is_some_and(|next| next.is_ascii_alphabetic() || matches!(next, '/' | '!'));
            match self.state {
                // Only `<` before a name, `/`, or `!` opens a tag; `rate < 5` is text.
                StripState::Text if opens_tag => {
                    self.state = StripState::Tag;
                    self.recent.clear();
                    self.quote = None;
                }
                StripState::Text => text.push(ch),
                StripState::Tag => {
                    self.remember(ch);
                    if self.recent == "!--" {
                        self.state = StripState::Comment;
                    } else if let Some(quote) = self.quote {
                        if ch == quote {
                            self.quote = None;
                        }
                    } else if ch == '"' || ch == '\'' {
                        self.quote = Some(ch);
                    } else if ch == '>' {
                        self.state = StripState::Text;
                    }
                }
                StripState::Comment => {
                    self.remember(ch);
                    if self.recent.ends_with("-->") {
                        self.state = StripState::Text;
                    }
                }
            }
        }
        text
    }

    fn remember(&mut self, ch: char) {
        self.recent.push(ch);
        if self.recent.chars().count() > 3 {
            self.recent.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_github_flavored_markdown() {
        let html = to_html(
            "## Why\n\nSome **bold** and ~~gone~~ `code`.\n\n- [x] done\n- [ ] todo\n\n| a | b |\n| - | - |\n| 1 | 2 |\n",
        );
        assert!(html.contains("<h2>Why</h2>"), "{html}");
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<del>gone</del>"));
        assert!(html.contains("<code>code</code>"));
        assert!(html.contains(r#"<input disabled="" type="checkbox" checked=""/>"#));
        assert!(html.contains("<table>"));
    }

    #[test]
    fn raw_html_keeps_only_its_text() {
        let html = to_html(
            "<details><summary>More</summary>\n\nhidden\n\n</details>\n\ninline <script>alert(1)</script> text\n",
        );
        assert!(!html.contains("<details"), "{html}");
        assert!(!html.contains("<script"), "{html}");
        assert!(html.contains("More"));
        assert!(html.contains("hidden"));
    }

    #[test]
    fn html_spanning_lines_keeps_only_its_text() {
        let html = to_html(
            "<!--\ntemplate hint\n-->\n\n<img\nsrc=\"x.png\"\nalt=\"a > b\">\n\n<p>kept</p>\n\n<!-- a > b -->after\n",
        );
        assert!(!html.contains("template hint"), "{html}");
        assert!(!html.contains("--&gt;"), "{html}");
        assert!(!html.contains("src"), "{html}");
        assert!(html.contains("kept"), "{html}");
        assert!(html.contains("after"), "{html}");
        assert!(!html.contains("a &gt; b"), "{html}");
    }

    #[test]
    fn a_bare_angle_bracket_in_html_is_text() {
        let html = to_html("<div>rate < 5, don't</div>\n\nmiddle\n\n<div>later</div>\n");
        assert!(html.contains("rate &lt; 5, don"), "{html}");
        assert!(html.contains("later"), "{html}");
        // An unclosed tag ends with its block rather than swallowing what follows.
        let html = to_html("<div title=\"open\n\nnext paragraph\n");
        assert!(html.contains("next paragraph"), "{html}");
    }

    #[test]
    fn script_links_are_neutralized() {
        let html =
            to_html("[x](javascript:alert(1)) [y](JavaScript:alert(1)) [z](data:text/html,hi)");
        assert!(!html.to_ascii_lowercase().contains("javascript:"), "{html}");
        assert!(!html.contains("data:"), "{html}");
        let html =
            to_html("[a](https://example.com) [b](#anchor) [c](docs/x.md) [d](mailto:me@x.y)");
        for href in [
            "https://example.com",
            "#anchor",
            "docs/x.md",
            "mailto:me@x.y",
        ] {
            assert!(html.contains(&format!(r#"href="{href}""#)), "{html}");
        }
    }

    #[test]
    fn images_become_links() {
        let html = to_html("![screenshot](https://example.com/a.png)");
        assert!(!html.contains("<img"), "{html}");
        assert!(html.contains(r#"<a href="https://example.com/a.png">screenshot</a>"#));
    }
}
