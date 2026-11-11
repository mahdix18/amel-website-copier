use url::Url;

use crate::{Link, LinkKind, resolve};

/// Every URL a stylesheet references, resolved against `base`.
pub fn extract_css(css: &str, base: &Url) -> Vec<Link> {
    let mut links = Vec::new();
    walk(css, |raw| {
        if let Some(url) = resolve(base, raw) {
            links.push(Link {
                url,
                kind: LinkKind::Asset,
            });
        }
        None
    });
    links
}

/// Replaces each URL for which `map` returns a value.
pub fn rewrite_css(
    css: &str,
    base: &Url,
    mut map: impl FnMut(&Url, LinkKind) -> Option<String>,
) -> String {
    walk(css, |raw| {
        resolve(base, raw).and_then(|url| map(&url, LinkKind::Asset))
    })
}

/// Visits the URLs in `url(…)`, `@import "…"` and `image-set("…")`, skipping
/// comments and unrelated strings, and returns the CSS with every URL that
/// `on_url` mapped replaced. All delimiters are ASCII, so byte offsets
/// always fall on character boundaries.
pub(crate) fn walk(css: &str, mut on_url: impl FnMut(&str) -> Option<String>) -> String {
    let bytes = css.as_bytes();
    let mut out = Splice::new(css);
    let mut i = 0;
    // Non-zero inside image-set(…); tracks nested parentheses.
    let mut image_set_depth = 0usize;
    // Set after `@import` until the URL that follows it.
    let mut after_import = false;

    'scan: while i < bytes.len() {
        let byte = bytes[i];

        if byte == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i = find(bytes, i + 2, b"*/").map_or(bytes.len(), |end| end + 2);
            continue;
        }

        if byte == b'"' || byte == b'\'' {
            let end = string_end(bytes, i);
            if after_import || image_set_depth > 0 {
                let inner_end = if end > i + 1 && bytes[end - 1] == byte {
                    end - 1
                } else {
                    end
                };
                out.replace(i + 1, inner_end, &mut on_url);
            }
            after_import = false;
            i = end;
            continue;
        }

        if byte == b'@' && starts_with_ci(&bytes[i..], b"@import") {
            after_import = true;
            i += b"@import".len();
            continue;
        }

        if at_word_start(bytes, i) {
            if starts_with_ci(&bytes[i..], b"url(") {
                i = url_function(bytes, i + b"url(".len(), &mut out, &mut on_url);
                after_import = false;
                continue;
            }
            for name in [&b"-webkit-image-set("[..], b"image-set("] {
                if starts_with_ci(&bytes[i..], name) {
                    image_set_depth = 1;
                    i += name.len();
                    continue 'scan;
                }
            }
        }

        match byte {
            b'(' if image_set_depth > 0 => image_set_depth += 1,
            b')' if image_set_depth > 0 => image_set_depth -= 1,
            b';' | b'{' | b'}' => after_import = false,
            _ => {}
        }
        i += 1;
    }

    out.finish()
}

/// Handles the inside of `url(`, starting just after the parenthesis.
/// Returns the offset just past the closing parenthesis.
fn url_function(
    bytes: &[u8],
    start: usize,
    out: &mut Splice<'_>,
    on_url: &mut impl FnMut(&str) -> Option<String>,
) -> usize {
    let mut j = start;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }

    if let Some(&quote @ (b'"' | b'\'')) = bytes.get(j) {
        let end = string_end(bytes, j);
        let inner_end = if end > j + 1 && bytes[end - 1] == quote {
            end - 1
        } else {
            end
        };
        out.replace(j + 1, inner_end, on_url);
        return find(bytes, end, b")").map_or(bytes.len(), |close| close + 1);
    }

    let close = find(bytes, j, b")").unwrap_or(bytes.len());
    let mut end = close;
    while end > j && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    out.replace(j, end, on_url);
    (close + 1).min(bytes.len())
}

/// Offset just past the string starting at `start` (a quote), honouring
/// backslash escapes. An unterminated string ends at a newline or EOF.
fn string_end(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'\n' => return i,
            b if b == quote => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

fn starts_with_ci(bytes: &[u8], prefix: &[u8]) -> bool {
    bytes.len() >= prefix.len() && bytes[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn at_word_start(bytes: &[u8], i: usize) -> bool {
    i == 0 || !matches!(bytes[i - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_')
}

/// Builds the output by copying the input and splicing in replacements.
struct Splice<'a> {
    input: &'a str,
    out: String,
    copied: usize,
}

impl<'a> Splice<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            out: String::new(),
            copied: 0,
        }
    }

    fn replace(
        &mut self,
        start: usize,
        end: usize,
        on_url: &mut impl FnMut(&str) -> Option<String>,
    ) {
        if let Some(new) = on_url(&self.input[start..end]) {
            self.out.push_str(&self.input[self.copied..start]);
            self.out.push_str(&new);
            self.copied = end;
        }
    }

    fn finish(mut self) -> String {
        if self.copied == 0 {
            return self.input.to_owned();
        }
        self.out.push_str(&self.input[self.copied..]);
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::walk;

    fn urls(css: &str) -> Vec<String> {
        let mut found = Vec::new();
        walk(css, |raw| {
            found.push(raw.to_owned());
            None
        });
        found
    }

    #[test]
    fn finds_every_url_form() {
        let css = r#"
            @import "a.css";
            @import url('b.css') screen;
            body { background: url(  c.png  ) }
            .x { background: url("d e.png") }
            .y { background-image: image-set("f.png" 1x, url(g.png) 2x); }
            .z { background-image: -webkit-image-set('h.png' 1x); }
        "#;
        assert_eq!(
            urls(css),
            [
                "a.css", "b.css", "c.png", "d e.png", "f.png", "g.png", "h.png"
            ]
        );
    }

    #[test]
    fn ignores_comments_and_unrelated_strings() {
        let css =
            r#"/* url(no.png) */ a::after { content: "url(nope.png)"; font-family: "Inter"; }"#;
        assert!(urls(css).is_empty());
    }

    #[test]
    fn needs_a_word_boundary_before_url() {
        assert!(urls("a { --myurl(x.png) }").is_empty());
    }

    #[test]
    fn handles_escaped_quotes_and_non_ascii() {
        assert_eq!(
            urls(r#"a { b: url("x\"y.png") } /* café */ c { d: url(é.png) }"#),
            [r#"x\"y.png"#, "é.png"]
        );
    }

    #[test]
    fn unchanged_input_is_returned_as_is() {
        let css = "a { color: red }";
        assert_eq!(walk(css, |_| None), css);
    }

    #[test]
    fn splices_replacements_preserving_quotes() {
        let out = walk(r#"a{b:url("x.png")} c{d:url( y.png )}"#, |raw| {
            Some(format!("local/{raw}"))
        });
        assert_eq!(out, r#"a{b:url("local/x.png")} c{d:url( local/y.png )}"#);
    }

    #[test]
    fn unterminated_input_does_not_panic() {
        for css in [
            "url(",
            "url(\"abc",
            "@import \"x",
            "image-set(",
            "/* open",
            "'",
        ] {
            walk(css, |_| Some(String::new()));
        }
    }
}
