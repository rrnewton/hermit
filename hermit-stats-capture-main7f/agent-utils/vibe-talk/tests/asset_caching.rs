//! `#223 asset-caching`: what a browser may keep of the web app, asked through the whole router.
//!
//! Through the whole router rather than the handler, because the part most likely to be wrong is
//! the interaction: compression runs OUTSIDE the handler that sets the validators, so a shell's
//! ETag, its `Vary`, and its `304` are only worth checking as the compression layer leaves them.
//!
//! What a launch of the installed app is meant to cost: one conditional request for the shell,
//! answered `304` when nothing was deployed, and nothing at all for the scripts, stylesheets and
//! icons it names, which are immutable under URLs that carry their own content hash.

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;
use vibe_talk::http::router;

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const REVALIDATE: &str = "no-cache";

/// The pages and the manifest: revalidated, never content-addressed.
const DOCUMENTS: [&str; 3] = ["/", "/voice", "/manifest.webmanifest"];

/// Every static file the router serves by a plain URL, and so every one an older shell may name.
const FILES: [&str; 9] = [
    "/voice.js",
    "/contract.js",
    "/voice.css",
    "/style.css",
    "/app.js",
    "/icons/icon.svg",
    "/icons/icon-192.png",
    "/icons/icon-512.png",
    "/icons/maskable-512.png",
];

fn app() -> axum::Router {
    let (state, _discord, _voice) = vibe_talk::testing::state_parts();
    router(state)
}

/// `GET uri` with `headers`, and the whole answer.
async fn fetch(
    app: &axum::Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method("GET").uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("router responds");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

/// Every `Vary` value, split, lowercased.
fn varies_on(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("vary")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .collect()
}

/// The first 128 bits of SHA-256, in lowercase hex: what the server names a file by.
fn content_hash(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Every same-origin `src="…"` or `href="…"` in a page that names something other than a page.
fn file_references(page: &str) -> Vec<String> {
    let mut found = Vec::new();
    for attribute in ["src", "href"] {
        let needle = format!("{attribute}=\"");
        let mut from = 0;
        while let Some(offset) = page[from..].find(&needle) {
            let at = from + offset;
            from = at + needle.len();
            if !page[..at].ends_with(|c: char| c.is_ascii_whitespace()) {
                continue;
            }
            let end = page[from..].find('"').expect("a closed attribute");
            let value = &page[from..from + end];
            if value.starts_with('/') && !DOCUMENTS.contains(&value) {
                found.push(value.to_owned());
            }
        }
    }
    found
}

#[tokio::test]
async fn every_file_a_shell_names_is_immutable_under_a_url_carrying_the_hash_of_its_bytes() {
    let app = app();
    for (shell, expected) in [
        (
            "/voice",
            &[
                "/style.css",
                "/voice.css",
                "/contract.js",
                "/voice.js",
                "/icons/icon-192.png",
                "/icons/icon.svg",
            ][..],
        ),
        (
            "/",
            &[
                "/style.css",
                "/app.js",
                "/icons/icon-192.png",
                "/icons/icon.svg",
            ][..],
        ),
    ] {
        let (status, _, page) = fetch(&app, shell, &[]).await;
        assert_eq!(status, StatusCode::OK, "{shell}");
        let page = String::from_utf8(page).expect("utf-8");
        let references = file_references(&page);
        let mut named: Vec<&str> = Vec::new();
        for reference in &references {
            // A reference the shell was not rewritten to hash would still load, by its plain URL,
            // revalidated on every launch — the cost this change exists to remove.
            let (file, version) = reference
                .split_once("?v=")
                .unwrap_or_else(|| panic!("{shell} names {reference} without its hash"));
            named.push(file);
            let (status, headers, body) =
                fetch(&app, reference, &[("accept-encoding", "identity")]).await;
            assert_eq!(status, StatusCode::OK, "{reference}");
            assert_eq!(header(&headers, "cache-control"), IMMUTABLE, "{reference}");
            assert_eq!(
                content_hash(&body),
                version,
                "{reference} is not named by the hash of the bytes it serves"
            );
            assert_eq!(
                header(&headers, "etag"),
                format!("W/\"{version}\""),
                "{reference}"
            );
            // The plain URL an older shell names: the same bytes, the same validator, revalidated.
            let (status, plain, same) = fetch(&app, file, &[("accept-encoding", "identity")]).await;
            assert_eq!(status, StatusCode::OK, "{file}");
            assert_eq!(header(&plain, "cache-control"), REVALIDATE, "{file}");
            assert_eq!(header(&plain, "etag"), header(&headers, "etag"), "{file}");
            assert!(same == body, "{file} and {reference} serve different bytes");
        }
        for file in expected {
            assert!(
                named.contains(file),
                "{shell} no longer names {file}: {references:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_shell_revalidates_to_a_304_whichever_encoding_it_was_kept_in() {
    let app = app();
    for document in DOCUMENTS {
        let mut tags = Vec::new();
        let mut identity = Vec::new();
        for (offer, encoding) in [("br, gzip", "br"), ("gzip", "gzip"), ("identity", "")] {
            let (status, headers, body) =
                fetch(&app, document, &[("accept-encoding", offer)]).await;
            assert_eq!(status, StatusCode::OK, "{document} {offer}");
            assert_eq!(
                header(&headers, "content-encoding"),
                encoding,
                "{document} {offer}: compression must survive caching"
            );
            assert_eq!(
                header(&headers, "cache-control"),
                REVALIDATE,
                "{document} {offer}"
            );
            assert_eq!(
                varies_on(&headers),
                ["accept-encoding"],
                "{document} {offer}"
            );
            tags.push(header(&headers, "etag").to_owned());
            if encoding.is_empty() {
                identity = body;
            }
        }
        // Weak, and the same tag on all three: the bytes differ by encoding, the content does not.
        let tag = &tags[0];
        assert!(
            tags.iter().all(|other| other == tag),
            "{document}: {tags:?}"
        );
        assert_eq!(
            tag,
            &format!("W/\"{}\"", content_hash(&identity)),
            "{document}: the tag is the hash of the body as generated"
        );
        // A copy kept in ANY encoding revalidates, asked in any encoding: one small request.
        for offer in ["br, gzip", "gzip", "identity"] {
            for held in [tag.clone(), tag.trim_start_matches("W/").to_owned()] {
                let (status, headers, body) = fetch(
                    &app,
                    document,
                    &[("accept-encoding", offer), ("if-none-match", &held)],
                )
                .await;
                assert_eq!(
                    status,
                    StatusCode::NOT_MODIFIED,
                    "{document} {offer} {held}"
                );
                assert!(body.is_empty(), "{document}: a 304 has no body");
                assert_eq!(header(&headers, "etag"), tag, "{document}");
                assert_eq!(header(&headers, "cache-control"), REVALIDATE, "{document}");
                assert_eq!(varies_on(&headers), ["accept-encoding"], "{document}");
                assert!(!headers.contains_key("content-encoding"), "{document}");
            }
        }
        // A copy from before a deploy does not match, and gets the new body.
        let (status, _, body) = fetch(
            &app,
            document,
            &[
                ("accept-encoding", "identity"),
                ("if-none-match", "W/\"0123456789abcdef0123456789abcdef\""),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{document}");
        assert!(body == identity, "{document}");
    }
}

#[tokio::test]
async fn an_app_installed_before_this_change_still_loads_and_revalidates() {
    // An installed app, or a shell already cached, names files by their plain URLs; a page left
    // open across a deploy names them by a hash that is no longer current. Neither may break, and
    // neither may be told its bytes are immutable.
    let app = app();
    for file in FILES {
        let (status, headers, body) = fetch(&app, file, &[("accept-encoding", "identity")]).await;
        assert_eq!(status, StatusCode::OK, "{file}");
        assert_eq!(header(&headers, "cache-control"), REVALIDATE, "{file}");
        let tag = header(&headers, "etag").to_owned();
        assert_eq!(tag, format!("W/\"{}\"", content_hash(&body)), "{file}");

        let (status, headers, _) = fetch(&app, file, &[("if-none-match", &tag)]).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED, "{file}");
        assert_eq!(header(&headers, "cache-control"), REVALIDATE, "{file}");

        let stale = format!("{file}?v=0123456789abcdef0123456789abcdef");
        let (status, headers, same) = fetch(&app, &stale, &[("accept-encoding", "identity")]).await;
        assert_eq!(status, StatusCode::OK, "{stale}");
        assert_eq!(header(&headers, "cache-control"), REVALIDATE, "{stale}");
        assert!(same == body, "{stale}");
    }
    let (status, _, _) = fetch(&app, "/icons/nope.png", &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_manifest_names_its_icons_by_their_plain_urls() {
    // The installed app's identity: an icon URL that moved with every edit to the artwork would
    // make each edit an identity change for every installed copy. See `src/http/assets.rs`.
    let app = app();
    let (_, _, body) = fetch(&app, "/manifest.webmanifest", &[]).await;
    let manifest: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    let icons = manifest["icons"].as_array().expect("icons");
    assert!(!icons.is_empty());
    for icon in icons {
        let src = icon["src"].as_str().expect("src");
        assert!(!src.contains('?'), "{src}");
        let (status, headers, _) = fetch(&app, src, &[]).await;
        assert_eq!(status, StatusCode::OK, "{src}");
        assert_eq!(header(&headers, "cache-control"), REVALIDATE, "{src}");
    }
}
