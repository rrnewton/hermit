//! The phone web app's static files, and what a browser is told it may keep.
//!
//! `#223 asset-caching`. Every one of these used to be answered `Cache-Control: no-store`, so each
//! launch of the installed app downloaded the whole page again — the page script alone is 262 KB
//! on the wire after compression. Now:
//!
//! * **Scripts, stylesheets and icons are content-addressed.** Each file's URL carries the
//!   SHA-256 of its bytes (128 bits of it, in hex), `/voice.js?v=<hash>`, and under that URL it is
//!   answered `public, max-age=31536000, immutable`: a browser never asks for it again, and does
//!   not need to, because different bytes would have a different URL.
//! * **The HTML shells (`/` and `/voice`) are generated, not edited.** At startup each page's
//!   `src="…"` and `href="…"` references to those files are rewritten to the hashed URLs, so no
//!   hash is ever written into a file by hand and none can go stale.
//! * **The shells and the install manifest are revalidated on every load**: `no-cache`, with an
//!   ETag that is the hash of the generated body, and `If-None-Match` answered `304`. A deploy
//!   that changed any file changes the shell that names it, so the next launch gets the new shell
//!   in one small request and then downloads only the files whose hashes moved.
//! * **Old URLs keep working.** An app installed earlier, or a shell already in a cache, asks for
//!   the plain `/voice.js`. That URL is still served — the same current bytes, `no-cache` with the
//!   same ETag — and never 404s. So is a hashed URL whose hash is not the current one, which a page
//!   left open across a deploy can ask for: it gets the current bytes, revalidated, and is never
//!   told they are immutable, because a year-long entry under a URL that does not name its bytes
//!   would outlive a rollback to the version it does name.
//!
//! # Why `?v=<hash>` rather than `/assets/<hash>/voice.js`
//!
//! Every HTTP cache keys a response on its whole target URI, query included (RFC 9111 §2), so the
//! query is exactly as content-addressed as a path segment would be. What the query buys is that
//! the hashed and the plain URL are ONE route with one handler: "old URLs keep working" is then a
//! property of the route rather than a second table to keep in step, and no route was added — the
//! router's paths and methods, and the inventory `tests/scope_first.rs` keeps of them, are what
//! they were. The old objection — that some shared proxies decline to cache a URL with a query —
//! is about intermediaries; the cache a launch depends on is the browser's own, which has no such
//! rule.
//!
//! # Why the ETag is weak
//!
//! [`super::router`] compresses OUTSIDE this handler: one body goes out as Brotli, gzip or
//! identity depending on who asked, and the compression layer leaves `ETag` alone. A strong
//! validator promises byte-identical representations, so one strong tag on all three would be a
//! false promise (nginx's gzip filter weakens ETags for the same reason). A weak tag promises
//! semantic equivalence, which is true of the three, and `If-None-Match` uses the weak comparison
//! anyway (RFC 9110 §13.1.2), so revalidation answers `304` whichever encoding the cached copy was
//! in. The one thing a weak tag cannot validate is a byte range, and nothing here is fetched by
//! range.
//!
//! `Vary: Accept-Encoding` is set HERE, on the `200` and the `304` alike. The compression layer
//! adds it only to a body it considers compressing, a `304` has no body, and RFC 9110 §15.4.5
//! requires the `304` to carry the `Vary` the `200` would have.
//!
//! # Why there is still no service worker
//!
//! HTTP caching is enough for launch speed. Launching the installed app is a navigation to the
//! manifest's `start_url`, `/voice`: the browser revalidates the shell (a `304` of a few hundred
//! bytes when nothing was deployed) and takes every file it names from its own HTTP cache without
//! asking. A service worker could remove that one round trip and let the app open with no network
//! at all, and it is deliberately not used: CacheStorage outlives sign-out and is served by
//! whatever the worker decides (see "the offline message cache" in `web/voice.js`), and a worker
//! brings an update lifecycle in which a deploy is seen one launch late. The HTTP cache holds only
//! these public files. Every `/api/` and `/mcp` answer stays `no-store`, which
//! [`super::access_layer`] enforces at the edge and `tests/scope_first.rs` checks route by route.
//!
//! # The manifest names its icons by their plain URLs
//!
//! On purpose. The manifest is the installed app's identity, read by the browser's installer and
//! by its periodic update check rather than on each launch, and an icon URL that changed with
//! every edit to the artwork would make each such edit an identity change for every installed
//! copy. The plain icon URLs revalidate like any other plain URL.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::LazyLock;

use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;

use super::api;

/// The phone web app's page.
const INDEX_HTML: &str = include_str!("../../web/index.html");
/// The minimal page that starts an authenticated conversation.
const VOICE_HTML: &str = include_str!("../../web/voice.html");
/// The `/voice` page's script.
pub(crate) const VOICE_JS: &str = include_str!("../../web/voice.js");
const CONTRACT_JS: &str = include_str!("../../web/contract.js");
const VOICE_CSS: &str = include_str!("../../web/voice.css");
/// The phone web app's script.
pub(crate) const APP_JS: &str = include_str!("../../web/app.js");
const STYLE_CSS: &str = include_str!("../../web/style.css");
const MANIFEST: &str = include_str!("../../web/manifest.webmanifest");

const HTML: &str = "text/html; charset=utf-8";
const JAVASCRIPT: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";

/// A content-addressed URL: this exact name can only ever mean these exact bytes.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// Keep it, but ask before every use; the ETag makes the asking cheap.
const REVALIDATE: &str = "no-cache";

/// How a path is cached, and whether its body is rewritten.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    /// A script, stylesheet or icon. Named by the shells with its content hash, and immutable
    /// under that name.
    File,
    /// An HTML page. Its references to files are rewritten to their hashed URLs at startup, and it
    /// is revalidated on every load.
    Shell,
    /// The install manifest: revalidated like a shell, served exactly as written.
    Manifest,
}

/// One embedded file, by the path the router serves it at.
struct Source {
    path: &'static str,
    content_type: &'static str,
    body: &'static [u8],
    role: Role,
}

const fn source(
    path: &'static str,
    content_type: &'static str,
    body: &'static [u8],
    role: Role,
) -> Source {
    Source {
        path,
        content_type,
        body,
        role,
    }
}

/// Everything static the router serves. A path here needs a route in [`super::router`] too; a
/// route without an entry here answers 404.
const SOURCES: &[Source] = &[
    source("/", HTML, INDEX_HTML.as_bytes(), Role::Shell),
    source("/voice", HTML, VOICE_HTML.as_bytes(), Role::Shell),
    source(
        "/manifest.webmanifest",
        "application/manifest+json; charset=utf-8",
        MANIFEST.as_bytes(),
        Role::Manifest,
    ),
    source("/app.js", JAVASCRIPT, APP_JS.as_bytes(), Role::File),
    source("/voice.js", JAVASCRIPT, VOICE_JS.as_bytes(), Role::File),
    // The generated wire-contract validators `/voice` loads before its script.
    source(
        "/contract.js",
        JAVASCRIPT,
        CONTRACT_JS.as_bytes(),
        Role::File,
    ),
    // Held apart from `style.css` on purpose: it turns the document into a fixed application frame
    // (`100dvh`, no page scroll), and `/` is an ordinary scrolling page that must not inherit that.
    source("/voice.css", CSS, VOICE_CSS.as_bytes(), Role::File),
    source("/style.css", CSS, STYLE_CSS.as_bytes(), Role::File),
    // Embedded artwork, available before the app is authenticated.
    source(
        "/icons/icon.svg",
        "image/svg+xml",
        include_bytes!("../../web/icons/icon.svg"),
        Role::File,
    ),
    source(
        "/icons/icon-192.png",
        "image/png",
        include_bytes!("../../web/icons/icon-192.png"),
        Role::File,
    ),
    source(
        "/icons/icon-512.png",
        "image/png",
        include_bytes!("../../web/icons/icon-512.png"),
        Role::File,
    ),
    source(
        "/icons/maskable-512.png",
        "image/png",
        include_bytes!("../../web/icons/maskable-512.png"),
        Role::File,
    ),
];

/// One path, ready to answer.
struct Entry {
    content_type: &'static str,
    body: Bytes,
    /// The content hash a [`Role::File`] is named by. `None` for a shell or the manifest, which
    /// keep their plain URLs and are revalidated instead.
    version: Option<String>,
    etag: HeaderValue,
}

/// Every static path and its answer, computed once from [`SOURCES`].
struct Site {
    entries: BTreeMap<&'static str, Entry>,
}

/// Computed on first use, which is once per process: the bytes are compiled in, so nothing about
/// them can change while it runs.
static SITE: LazyLock<Site> = LazyLock::new(|| Site::new(SOURCES));

/// `GET` any static path: the shells, the manifest, the scripts and stylesheets, and the icons.
///
/// See the module documentation for what each is told about caching, and why.
pub async fn serve(uri: Uri, headers: HeaderMap) -> Response {
    match SITE.answer(&uri, &headers) {
        Some(response) => response,
        None => api::not_found().await.into_response(),
    }
}

impl Site {
    fn new(sources: &[Source]) -> Self {
        // Every file's hash first: a shell cannot be generated until each file it may name has one.
        let versions: BTreeMap<&'static str, String> = sources
            .iter()
            .filter(|source| source.role == Role::File)
            .map(|file| (file.path, digest(file.body)))
            .collect();
        let entries = sources
            .iter()
            .map(|source| {
                let (body, version) = match source.role {
                    Role::File => (
                        Bytes::from_static(source.body),
                        versions.get(source.path).cloned(),
                    ),
                    Role::Shell => (
                        Bytes::from(link(&String::from_utf8_lossy(source.body), &versions)),
                        None,
                    ),
                    Role::Manifest => (Bytes::from_static(source.body), None),
                };
                (source.path, Entry::new(source.content_type, body, version))
            })
            .collect();
        Self { entries }
    }

    /// The answer for `uri`, or `None` when no static file lives at its path.
    fn answer(&self, uri: &Uri, headers: &HeaderMap) -> Option<Response> {
        let entry = self.entries.get(uri.path())?;
        // Immutable only under EXACTLY the URL the current shell names. No query, an older hash, or
        // anything added to it gets the same bytes revalidated instead; see the module comment.
        let named = entry
            .version
            .as_deref()
            .is_some_and(|version| uri.query() == Some(format!("v={version}").as_str()));
        let cache_control = HeaderValue::from_static(if named { IMMUTABLE } else { REVALIDATE });
        let validators = [
            (header::ETAG, entry.etag.clone()),
            (header::CACHE_CONTROL, cache_control),
            (header::VARY, HeaderValue::from_static("accept-encoding")),
        ];
        if already_held(headers, &entry.etag) {
            return Some((StatusCode::NOT_MODIFIED, validators).into_response());
        }
        Some(
            (
                validators,
                [(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static(entry.content_type),
                )],
                entry.body.clone(),
            )
                .into_response(),
        )
    }
}

impl Entry {
    fn new(content_type: &'static str, body: Bytes, version: Option<String>) -> Self {
        // A shell's tag is the hash of the body as GENERATED, so it moves whenever any file the
        // shell names does. A file's is its own content hash, the same one its URL carries.
        let tag = version.clone().unwrap_or_else(|| digest(&body));
        let etag = HeaderValue::try_from(format!("W/\"{tag}\""))
            .expect("hex digits in quotes are a valid header value");
        Self {
            content_type,
            body,
            version,
            etag,
        }
    }
}

/// Rewrite a page's `src="/x"` and `href="/x"` references to the hashed URL of `/x`.
///
/// Only those exact attribute forms, quoted, are touched: a quoted path elsewhere in the page —
/// prose, an inline script — is not a reference, and a bare `/voice` link is not `/voice.js`.
fn link(template: &str, versions: &BTreeMap<&'static str, String>) -> String {
    let mut page = template.to_owned();
    for (path, version) in versions {
        for attribute in ["src", "href"] {
            page = page.replace(
                &format!("{attribute}=\"{path}\""),
                &format!("{attribute}=\"{path}?v={version}\""),
            );
        }
    }
    page
}

/// Whether the client already holds the representation tagged `etag`.
///
/// RFC 9110 §13.1.2: `If-None-Match` uses the WEAK comparison, so `W/` on either side is ignored,
/// and `*` matches any current representation — which every path here has.
fn already_held(headers: &HeaderMap, etag: &HeaderValue) -> bool {
    let ours = opaque(etag.to_str().unwrap_or_default());
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|tag| tag == "*" || opaque(tag) == ours)
}

fn opaque(tag: &str) -> &str {
    tag.strip_prefix("W/").unwrap_or(tag)
}

/// The first 128 bits of the SHA-256 of `bytes`, in lowercase hex.
fn digest(bytes: &[u8]) -> String {
    let hash = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut hex = String::with_capacity(32);
    for byte in &hash.as_ref()[..16] {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    // That every reference in the REAL shells is rewritten, and served immutable under it, is
    // checked through the whole router in `tests/asset_caching.rs`.

    fn get(site: &Site, uri: &str, if_none_match: Option<&str>) -> Response {
        let mut headers = HeaderMap::new();
        if let Some(tag) = if_none_match {
            headers.insert(
                header::IF_NONE_MATCH,
                HeaderValue::from_str(tag).expect("tag"),
            );
        }
        site.answer(&uri.parse().expect("uri"), &headers)
            .expect("a static path")
    }

    fn header_of(response: &Response, name: header::HeaderName) -> &str {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    }

    const PAGE: &[u8] =
        br#"<link href="/a.css"><script src="/a.js"></script><a href="/a">"/a.js"</a>"#;

    fn site(script: &'static [u8]) -> Site {
        Site::new(&[
            source("/a", HTML, PAGE, Role::Shell),
            source("/a.js", JAVASCRIPT, script, Role::File),
            source("/a.css", CSS, b"body {}", Role::File),
        ])
    }

    #[test]
    fn a_shell_names_each_file_by_its_hash_and_moves_when_a_file_does() {
        let before = site(b"one();");
        let after = site(b"two();");
        let shell =
            |site: &Site| String::from_utf8(site.entries["/a"].body.to_vec()).expect("utf-8");
        let (old, new) = (shell(&before), shell(&after));
        let script = |site: &Site| site.entries["/a.js"].version.clone().expect("a file");
        assert!(
            old.contains(&format!("src=\"/a.js?v={}\"", script(&before))),
            "{old}"
        );
        assert!(
            new.contains(&format!("src=\"/a.js?v={}\"", script(&after))),
            "{new}"
        );
        assert_ne!(script(&before), script(&after));
        // The stylesheet did not change, so neither did its URL: a deploy re-downloads only what
        // moved.
        assert_eq!(
            before.entries["/a.css"].version,
            after.entries["/a.css"].version
        );
        // A page link is not a file reference, and neither is the same path quoted in prose.
        assert!(new.contains(r#"<a href="/a">"/a.js"</a>"#), "{new}");
        assert_ne!(before.entries["/a"].etag, after.entries["/a"].etag);
    }

    #[test]
    fn only_the_current_hash_is_immutable_and_every_other_spelling_revalidates() {
        let site = site(b"one();");
        let version = site.entries["/a.js"].version.clone().expect("a file");
        let named = get(&site, &format!("/a.js?v={version}"), None);
        assert_eq!(named.status(), StatusCode::OK);
        assert_eq!(header_of(&named, header::CACHE_CONTROL), IMMUTABLE);
        for uri in [
            "/a.js".to_owned(),
            "/a.js?v=0123456789abcdef0123456789abcdef".to_owned(),
            format!("/a.js?v={version}&x=1"),
            format!("/a.js?x=1&v={version}"),
        ] {
            let response = get(&site, &uri, None);
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(
                header_of(&response, header::CACHE_CONTROL),
                REVALIDATE,
                "{uri}"
            );
            assert_eq!(
                header_of(&response, header::ETAG),
                header_of(&named, header::ETAG),
                "{uri}"
            );
        }
        // A shell is never immutable, whatever it is asked with.
        assert_eq!(
            header_of(
                &get(&site, &format!("/a?v={version}"), None),
                header::CACHE_CONTROL
            ),
            REVALIDATE
        );
    }

    #[test]
    fn if_none_match_is_the_weak_comparison() {
        let site = site(b"one();");
        let etag = site.entries["/a"].etag.to_str().expect("ascii").to_owned();
        assert!(etag.starts_with("W/\"") && etag.ends_with('"'), "{etag}");
        let strong = etag.trim_start_matches("W/").to_owned();
        for held in [
            etag.clone(),
            strong,
            "*".to_owned(),
            format!("\"other\", {etag}"),
        ] {
            let response = get(&site, "/a", Some(&held));
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{held}");
            assert_eq!(header_of(&response, header::ETAG), etag);
            assert_eq!(header_of(&response, header::CACHE_CONTROL), REVALIDATE);
            assert_eq!(header_of(&response, header::VARY), "accept-encoding");
            assert!(response.headers().get(header::CONTENT_TYPE).is_none());
        }
        for held in ["W/\"other\"", "\"\"", ""] {
            assert_eq!(
                get(&site, "/a", Some(held)).status(),
                StatusCode::OK,
                "{held}"
            );
        }
    }
}
