//! `srcset` parsing, close enough to the HTML spec for real-world markup:
//! URLs are whitespace-delimited, so a comma inside a URL survives.

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

/// Splits a `srcset` value into `(url, descriptor)` candidates.
pub(crate) fn parse(value: &str) -> Vec<(&str, &str)> {
    let bytes = value.as_bytes();
    let mut candidates = Vec::new();
    let mut pos = 0;
    loop {
        while pos < bytes.len() && (is_space(bytes[pos]) || bytes[pos] == b',') {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }

        let url_start = pos;
        while pos < bytes.len() && !is_space(bytes[pos]) {
            pos += 1;
        }
        let mut url = &value[url_start..pos];

        let descriptor = if url.ends_with(',') {
            // "a.png," ends the candidate with no descriptor.
            url = url.trim_end_matches(',');
            ""
        } else {
            let start = pos;
            let mut depth = 0usize;
            while pos < bytes.len() {
                match bytes[pos] {
                    b'(' => depth += 1,
                    b')' => depth = depth.saturating_sub(1),
                    b',' if depth == 0 => break,
                    _ => {}
                }
                pos += 1;
            }
            value[start..pos].trim()
        };

        if !url.is_empty() {
            candidates.push((url, descriptor));
        }
    }
    candidates
}

/// Rewrites each candidate's URL. Returns `None` when nothing changed, so
/// callers can leave the attribute byte-for-byte intact.
pub(crate) fn rewrite(value: &str, mut map: impl FnMut(&str) -> Option<String>) -> Option<String> {
    let mut changed = false;
    let parts: Vec<String> = parse(value)
        .into_iter()
        .map(|(url, descriptor)| {
            let url = match map(url) {
                Some(new) => {
                    changed = true;
                    new
                }
                None => url.to_owned(),
            };
            if descriptor.is_empty() {
                url
            } else {
                format!("{url} {descriptor}")
            }
        })
        .collect();
    changed.then(|| parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parses_density_and_width_descriptors() {
        assert_eq!(
            parse("a.png 1x, b.png 2x"),
            [("a.png", "1x"), ("b.png", "2x")]
        );
        assert_eq!(
            parse("  small.jpg 480w ,large.jpg 1080w"),
            [("small.jpg", "480w"), ("large.jpg", "1080w")]
        );
    }

    #[test]
    fn keeps_commas_inside_urls() {
        assert_eq!(
            parse("img.php?size=1,2 1x, other.png"),
            [("img.php?size=1,2", "1x"), ("other.png", "")]
        );
    }

    #[test]
    fn trailing_comma_ends_a_candidate() {
        assert_eq!(parse("a.png, b.png 2x"), [("a.png", ""), ("b.png", "2x")]);
        // Per spec a URL runs to whitespace, so an inner comma stays in it.
        assert_eq!(parse("a.png,b.png 2x"), [("a.png,b.png", "2x")]);
    }

    #[test]
    fn empty_and_blank_values() {
        assert!(parse("").is_empty());
        assert!(parse(" , ,").is_empty());
    }
}
