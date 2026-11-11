#![allow(clippy::unwrap_used)]

use mc_extract::{LinkKind, Url, extract_css, extract_html, rewrite_css, rewrite_html};

fn base() -> Url {
    Url::parse("https://site.test/blog/post.html").unwrap()
}

fn found(html: &str) -> Vec<(String, LinkKind)> {
    extract_html(html.as_bytes(), None, &base())
        .unwrap()
        .into_iter()
        .map(|link| (link.url.to_string(), link.kind))
        .collect()
}

#[test]
fn extracts_pages_and_assets() {
    let html = r##"<!doctype html><html><head>
        <link rel="stylesheet" href="/s.css" integrity="sha384-x">
        <link rel="canonical" href="https://site.test/canonical">
        <link rel="shortcut icon" href="fav.ico">
        <meta http-equiv="refresh" content="30; url=next.html">
        <script src="app.js?v=2"></script>
        <style>body { background: url(bg.png) }</style>
      </head><body>
        <a href="other.html#top">x</a>
        <a href="mailto:a@b.test">mail</a> <a href="#local">frag</a> <a href="javascript:void(0)">js</a>
        <img src="a.png" srcset="a.png 1x, a@2x.png 2x" data-src="lazy.png">
        <video poster="poster.jpg"><source src="clip.mp4"></video>
        <iframe src="https://other.test/embed"></iframe>
        <div style="background-image: url('inline.png')"></div>
        <object data="doc.pdf"></object>
      </body></html>"##;

    let links = found(html);
    let page = |u: &str| (u.to_owned(), LinkKind::Page);
    let asset = |u: &str| (u.to_owned(), LinkKind::Asset);
    for expected in [
        asset("https://site.test/s.css"),
        asset("https://site.test/blog/fav.ico"),
        page("https://site.test/blog/next.html"),
        asset("https://site.test/blog/app.js?v=2"),
        asset("https://site.test/blog/bg.png"),
        page("https://site.test/blog/other.html#top"),
        asset("https://site.test/blog/a.png"),
        asset("https://site.test/blog/a@2x.png"),
        asset("https://site.test/blog/lazy.png"),
        asset("https://site.test/blog/poster.jpg"),
        asset("https://site.test/blog/clip.mp4"),
        page("https://other.test/embed"),
        asset("https://site.test/blog/inline.png"),
        asset("https://site.test/blog/doc.pdf"),
    ] {
        assert!(
            links.contains(&expected),
            "missing {expected:?} in {links:#?}"
        );
    }
    assert!(!links.iter().any(|(u, _)| u.contains("canonical")));
    assert!(
        !links
            .iter()
            .any(|(u, _)| u.starts_with("mailto") || u.starts_with("javascript"))
    );
    assert!(!links.iter().any(|(u, _)| u.ends_with("#local")));
}

#[test]
fn base_element_changes_resolution() {
    let links = found(r#"<head><base href="https://cdn.test/x/"></head><img src="i.png">"#);
    assert_eq!(
        links,
        [("https://cdn.test/x/i.png".to_owned(), LinkKind::Asset)]
    );
}

#[test]
fn rewrites_references_and_strips_what_breaks_local_copies() {
    let html = r#"<head><base href="/blog/"><meta http-equiv="Content-Security-Policy" content="default-src 'self'">
        <link rel="stylesheet" href="s.css" integrity="sha384-x"><meta http-equiv="refresh" content="0; url=next.html"></head>
        <a href="other.html#top">o</a><img srcset="a.png 1x, b.png 2x"><a href="https://ext.test/">e</a>"#;
    let out = rewrite_html(html.as_bytes(), None, &base(), |url, _| {
        (url.host_str() == Some("site.test")).then(|| format!("LOCAL{}", url.path()))
    })
    .unwrap();
    let out = String::from_utf8(out).unwrap();

    assert!(out.contains(r#"href="LOCAL/blog/s.css""#), "{out}");
    assert!(out.contains(r#"href="LOCAL/blog/other.html""#), "{out}");
    assert!(
        out.contains(r#"srcset="LOCAL/blog/a.png 1x, LOCAL/blog/b.png 2x""#),
        "{out}"
    );
    assert!(
        out.contains(r#"content="0; url=LOCAL/blog/next.html""#),
        "{out}"
    );
    assert!(out.contains(r#"href="https://ext.test/""#), "{out}");
    assert!(!out.contains("<base"), "{out}");
    assert!(!out.contains("integrity"), "{out}");
    assert!(!out.contains("Content-Security-Policy"), "{out}");
}

#[test]
fn rewrites_style_blocks_and_attributes() {
    let html = r#"<style>a { background: url("x.png") }</style><p style="background: url(y.png)">"#;
    let out = rewrite_html(html.as_bytes(), None, &base(), |url, _| {
        Some(format!("L-{}", url.path().rsplit('/').next().unwrap()))
    })
    .unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains(r#"url("L-x.png")"#), "{out}");
    assert!(out.contains("url(L-y.png)"), "{out}");
}

#[test]
fn attributes_needing_escapes_survive_rewriting() {
    let out = rewrite_html(br#"<a href="x.html">x</a>"#, None, &base(), |_, _| {
        Some(r#"a"b&c.html"#.to_owned())
    })
    .unwrap();
    // lol_html escapes only the quote; a bare `&` is valid in an HTML5 attribute.
    assert_eq!(
        String::from_utf8(out).unwrap(),
        r#"<a href="a&quot;b&c.html">x</a>"#
    );
}

#[test]
fn honours_non_utf8_charsets() {
    // "café" in windows-1252, with a link next to it.
    let mut html = b"<p>caf".to_vec();
    html.push(0xE9);
    html.extend_from_slice(br#"</p><a href="b.html">b</a>"#);
    let out = rewrite_html(&html, Some("windows-1252"), &base(), |_, _| {
        Some("c.html".to_owned())
    })
    .unwrap();
    assert!(out.windows(5).any(|w| w == [b'c', b'a', b'f', 0xE9, b'<']));
    assert!(String::from_utf8_lossy(&out).contains(r#"href="c.html""#));
}

#[test]
fn css_extraction_and_rewriting() {
    let css = r#"@import "theme.css"; body { background: url(../img/bg.png) }"#;
    let base = Url::parse("https://site.test/assets/main.css").unwrap();
    let links: Vec<_> = extract_css(css, &base)
        .into_iter()
        .map(|l| l.url.to_string())
        .collect();
    assert_eq!(
        links,
        [
            "https://site.test/assets/theme.css",
            "https://site.test/img/bg.png"
        ]
    );

    let out = rewrite_css(css, &base, |url, _| {
        Some(url.path().trim_start_matches('/').to_owned())
    });
    assert_eq!(
        out,
        r#"@import "assets/theme.css"; body { background: url(img/bg.png) }"#
    );
}
