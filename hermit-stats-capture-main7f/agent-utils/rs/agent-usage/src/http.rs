//! HTTPS GET through the system `curl`, with secret headers kept off the command line.
//!
//! Shelling out keeps the binary free of a TLS stack and inherits the host's proxy environment
//! (`https_proxy`); `-q` makes curl ignore any `.curlrc`. Headers are written to curl's stdin as a `--config -`
//! file, so a bearer token never appears in `ps` output or `/proc/<pid>/cmdline`.

use std::io::Write;
use std::process::{Command, Stdio};

/// A completed request: HTTP status (0 for `file://` URLs), response headers and the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// HTTP status code; `0` when the scheme has none (`file://`, used by tests).
    pub status: u16,
    /// Response headers of the final response, names lower-cased. Empty for `file://`.
    pub headers: Vec<(String, String)>,
    /// Response body as text.
    pub body: String,
}

impl Response {
    /// The first header with this (lower-case) name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Split curl's `--dump-header -` output into the final response's headers and the rest. A proxy
/// `CONNECT` reply or an interim `100 Continue` comes first as its own block, so blocks are peeled
/// off while the text still starts with a status line.
pub fn split_headers(text: &str) -> (Vec<(String, String)>, &str) {
    let mut rest = text;
    let mut headers = Vec::new();
    while rest.starts_with("HTTP/") {
        let (block, after) = match rest.find("\r\n\r\n") {
            Some(at) => (&rest[..at], &rest[at + 4..]),
            None => match rest.find("\n\n") {
                Some(at) => (&rest[..at], &rest[at + 2..]),
                None => (rest, ""),
            },
        };
        headers = block
            .lines()
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        rest = after;
    }
    (headers, rest)
}

/// Seconds named by a `Retry-After` header in its delta-seconds form; `None` for a missing,
/// non-positive or date-form value ("told us nothing").
pub fn retry_after_secs(value: Option<&str>) -> Option<i64> {
    value?.trim().parse::<i64>().ok().filter(|s| *s > 0)
}

fn config_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' | '\r' => {}
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `--config -` text for these headers.
pub fn header_config(headers: &[(&str, &str)]) -> String {
    let mut text = String::new();
    for (name, value) in headers {
        text.push_str("header = ");
        text.push_str(&config_quote(&format!("{name}: {value}")));
        text.push('\n');
    }
    text
}

/// GET `url` with `headers`, giving up after `timeout_secs`. `curl` is looked up on `PATH`
/// unless `AGENT_USAGE_CURL` names it.
pub fn get(url: &str, headers: &[(&str, &str)], timeout_secs: u64) -> Result<Response, String> {
    let curl = std::env::var("AGENT_USAGE_CURL").unwrap_or_else(|_| "curl".to_string());
    let mut cmd = Command::new(&curl);
    if url.starts_with("http") {
        cmd.args(["--dump-header", "-"]);
    }
    let mut child = cmd
        .args([
            "-q",
            "--silent",
            "--show-error",
            "--max-time",
            &timeout_secs.to_string(),
            "--write-out",
            "\n%{http_code}",
            "--config",
            "-",
            "--url",
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {curl}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(header_config(headers).as_bytes())
            .map_err(|e| format!("cannot write curl config: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("curl failed: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "curl exit {}: {}",
            out.status.code().unwrap_or(-1),
            err.trim().chars().take(200).collect::<String>()
        ));
    }
    let (headers, rest) = split_headers(&text);
    let (body, code) = match rest.rfind('\n') {
        Some(at) => (rest[..at].to_string(), rest[at + 1..].trim().to_string()),
        None => (String::new(), rest.trim().to_string()),
    };
    let status = code.parse::<u16>().unwrap_or(0);
    Ok(Response {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_quotes_and_strips_newlines() {
        let text = header_config(&[("A", "b\"c\\d\ne")]);
        assert_eq!(text, "header = \"A: b\\\"c\\\\de\"\n");
    }

    #[test]
    fn splits_proxy_and_final_header_blocks() {
        let text = "HTTP/1.1 200 Connection established\r\n\r\nHTTP/2 429\r\nRetry-After: 832\r\ncontent-type: application/json\r\n\r\n{\"error\":{}}\n429";
        let (headers, rest) = split_headers(text);
        assert_eq!(rest, "{\"error\":{}}\n429");
        let resp = Response {
            status: 429,
            headers,
            body: String::new(),
        };
        assert_eq!(resp.header("retry-after"), Some("832"));
        assert_eq!(resp.header("content-type"), Some("application/json"));
        let (headers, rest) = split_headers("{}\n200");
        assert!(headers.is_empty());
        assert_eq!(rest, "{}\n200");
    }

    #[test]
    fn retry_after_values() {
        assert_eq!(retry_after_secs(Some("832")), Some(832));
        assert_eq!(retry_after_secs(Some(" 5 ")), Some(5));
        assert_eq!(retry_after_secs(Some("0")), None);
        assert_eq!(
            retry_after_secs(Some("Wed, 21 Oct 2026 07:28:00 GMT")),
            None
        );
        assert_eq!(retry_after_secs(None), None);
    }

    #[test]
    fn reads_a_file_url() {
        let dir = std::env::temp_dir().join(format!("agent-usage-http-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("body.json");
        std::fs::write(&file, "{\"ok\":true}").unwrap();
        let resp = get(&format!("file://{}", file.display()), &[("X", "y")], 5).unwrap();
        assert_eq!(resp.status, 0);
        assert_eq!(resp.body, "{\"ok\":true}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
