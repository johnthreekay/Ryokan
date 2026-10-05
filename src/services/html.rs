use ammonia::Builder;
use std::collections::HashSet;

fn escape_html(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// `url` for an `href`, or `""` when it isn't http(s) or magnet.
/// Release links come from indexers and feeds, and a `javascript:` (or
/// `data:`) link rendered into an `href` runs in Ryokan's origin when
/// clicked. `static/js/search.js` has the same rule as `safeHref`.
pub fn safe_href(url: &str) -> &str {
    let trimmed = url.trim();
    let lower = trimmed.get(..8).unwrap_or(trimmed).to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("magnet:")
    {
        trimmed
    } else {
        ""
    }
}

pub fn sanitize_rich_description(raw: &str, treat_as_html: bool) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let normalized = trimmed.replace("\r\n", "\n").replace('\r', "\n");
    let fragment = if treat_as_html {
        normalized
    } else {
        escape_html(&normalized).replace("\n", "<br>\n")
    };

    let tags: HashSet<&str> = [
        "br",
        "p",
        "b",
        "strong",
        "i",
        "em",
        "u",
        "ul",
        "ol",
        "li",
        "blockquote",
    ]
    .into_iter()
    .collect();

    Builder::default().tags(tags).clean(&fragment).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_href_keeps_web_and_magnet_links_only() {
        for ok in [
            "https://nyaa.si/view/1",
            "HTTP://indexer.local/download?id=1",
            "magnet:?xt=urn:btih:aabbccddeeff00112233445566778899aabbccdd",
        ] {
            assert_eq!(safe_href(ok), ok);
        }
        assert_eq!(safe_href("  https://x.example/ "), "https://x.example/");
        for bad in [
            "javascript:alert(document.cookie)",
            "JavaScript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "vbscript:msgbox(1)",
            "//evil.example/x",
            "",
            "é",
        ] {
            assert_eq!(safe_href(bad), "", "{bad}");
        }
    }
}
