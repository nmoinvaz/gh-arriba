use anyhow::{Context, Result, bail};
use reqwest::Method;
use reqwest::blocking::{Client as Http, RequestBuilder, Response};
use reqwest::header::LINK;
use serde_json::{Value, json};
use std::process::Command;
use std::sync::OnceLock;

/// REST and GraphQL client authenticated with the token gh uses.
pub struct Client {
    http: Http,
    token: String,
    api: String,
    graphql: String,
    viewer: OnceLock<String>,
}

impl Client {
    pub fn new() -> Result<Self> {
        let host = std::env::var("GH_HOST")
            .ok()
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "github.com".to_string());
        let token = get_github_token(&host)?;
        // GitHub Enterprise Server serves the API under the host itself.
        let (api, graphql) = if host == "github.com" {
            (
                "https://api.github.com".to_string(),
                "https://api.github.com/graphql".to_string(),
            )
        } else {
            (
                format!("https://{host}/api/v3"),
                format!("https://{host}/api/graphql"),
            )
        };
        let http = Http::builder()
            .user_agent(concat!("gh-arriba/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("Failed to create HTTP client")?;
        Ok(Self {
            http,
            token,
            api,
            graphql,
            viewer: OnceLock::new(),
        })
    }

    /// Login of the authenticated user.
    pub fn viewer(&self) -> Result<&str> {
        if let Some(login) = self.viewer.get() {
            return Ok(login);
        }
        let data = self.graphql("query { viewer { login } }", json!({}))?;
        let login = data["viewer"]["login"]
            .as_str()
            .context("viewer login missing from response")?
            .to_string();
        Ok(self.viewer.get_or_init(|| login))
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("https://") {
            path.to_string()
        } else {
            format!("{}/{}", self.api, path.trim_start_matches('/'))
        }
    }

    fn request(&self, method: Method, url: &str) -> RequestBuilder {
        self.request_as(method, url, "application/vnd.github+json")
    }

    /// A request that accepts `media_type`. reqwest appends repeated headers, so each request
    /// sets Accept exactly once.
    fn request_as(&self, method: Method, url: &str, media_type: &str) -> RequestBuilder {
        self.http
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Accept", media_type)
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    fn send(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Response> {
        let url = self.url(path);
        let mut req = self.request(method.clone(), &url);
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = req
            .send()
            .with_context(|| format!("{method} {path} failed"))?;
        check(resp, &method, path)
    }

    /// GET a path relative to the API root, or an absolute API URL.
    pub fn get(&self, path: &str) -> Result<Value> {
        parse_json(self.send(Method::GET, path, None)?)
    }

    /// GET that maps 404 to None.
    pub fn get_opt(&self, path: &str) -> Result<Option<Value>> {
        let url = self.url(path);
        let resp = self
            .request(Method::GET, &url)
            .send()
            .with_context(|| format!("GET {path} failed"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        parse_json(check(resp, &Method::GET, path)?).map(Some)
    }

    /// GET every page of a list endpoint that returns a JSON array.
    pub fn get_all(&self, path: &str) -> Result<Vec<Value>> {
        self.paginate(path, None)
    }

    /// GET every page of a list endpoint that wraps its array in `key`, as in `{"jobs": [...]}`.
    pub fn get_all_in(&self, path: &str, key: &str) -> Result<Vec<Value>> {
        self.paginate(path, Some(key))
    }

    fn paginate(&self, path: &str, key: Option<&str>) -> Result<Vec<Value>> {
        let mut next = Some(with_per_page(&self.url(path)));
        let mut items = Vec::new();
        while let Some(url) = next {
            let resp = self.send(Method::GET, &url, None)?;
            next = next_link(&resp);
            let page = parse_json(resp)?;
            let array = match key {
                Some(k) => page[k].clone(),
                None => page,
            };
            match array {
                Value::Array(a) => items.extend(a),
                other => bail!("Expected a JSON array from {path}, got {other}"),
            }
        }
        Ok(items)
    }

    /// GET a text body, such as a job log. Redirects are followed.
    pub fn get_text(&self, path: &str) -> Result<String> {
        let resp = self.send(Method::GET, path, None)?;
        resp.text()
            .with_context(|| format!("Failed to read body of {path}"))
    }

    /// GET a binary body, such as an artifact zip, streaming it into `dest`. Redirects are
    /// followed. Returns the bytes written.
    pub fn download(&self, path: &str, dest: &std::path::Path) -> Result<u64> {
        let mut resp = self.send(Method::GET, path, None)?;
        let mut file = std::fs::File::create(dest)
            .with_context(|| format!("Failed to create {}", dest.display()))?;
        std::io::copy(&mut resp, &mut file)
            .with_context(|| format!("Failed to download {path} to {}", dest.display()))
    }

    /// GET file contents from the contents API as raw text.
    pub fn get_raw(&self, path: &str) -> Result<String> {
        let url = self.url(path);
        let resp = self
            .request_as(Method::GET, &url, "application/vnd.github.raw+json")
            .send()
            .with_context(|| format!("GET {path} failed"))?;
        check(resp, &Method::GET, path)?
            .text()
            .with_context(|| format!("Failed to read body of {path}"))
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value> {
        parse_json(self.send(Method::POST, path, Some(body))?)
    }

    pub fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        parse_json(self.send(Method::PATCH, path, Some(body))?)
    }

    pub fn put(&self, path: &str, body: &Value) -> Result<Value> {
        parse_json(self.send(Method::PUT, path, Some(body))?)
    }

    pub fn delete(&self, path: &str) -> Result<Value> {
        parse_json(self.send(Method::DELETE, path, None)?)
    }

    /// Run a GraphQL query and return its `data`. Any entry in `errors` fails the call.
    pub fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let body = json!({ "query": query, "variables": variables });
        let resp = self
            .request(Method::POST, &self.graphql)
            .json(&body)
            .send()
            .context("GraphQL request failed")?;
        let mut value = parse_json(check(resp, &Method::POST, "graphql")?)?;
        if let Some(errors) = value["errors"].as_array().filter(|e| !e.is_empty()) {
            let messages: Vec<&str> = errors
                .iter()
                .filter_map(|e| e["message"].as_str())
                .collect();
            bail!("GraphQL error: {}", messages.join("; "));
        }
        Ok(value["data"].take())
    }
}

/// Fail with GitHub's own message on a non-2xx response.
fn check(resp: Response, method: &Method, path: &str) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let text = resp.text().unwrap_or_default();
    let parsed: Option<Value> = serde_json::from_str(&text).ok();
    let mut message = parsed
        .as_ref()
        .and_then(|v| v["message"].as_str())
        .map(str::to_string)
        .unwrap_or(text);
    // 422 responses carry the specific validation failures in `errors`.
    if let Some(errors) = parsed.as_ref().and_then(|v| v["errors"].as_array()) {
        let details: Vec<String> = errors
            .iter()
            .map(|e| match e {
                Value::String(s) => s.clone(),
                other => other["message"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| other.to_string()),
            })
            .collect();
        if !details.is_empty() {
            message = format!("{message} ({})", details.join("; "));
        }
    }
    bail!(
        "GitHub API {} on {method} {path}: {message}",
        status.as_u16()
    )
}

/// Parse a JSON body, mapping an empty body (such as 204) to Null.
fn parse_json(resp: Response) -> Result<Value> {
    let text = resp.text().context("Failed to read response body")?;
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).context("Failed to parse JSON response")
}

fn with_per_page(url: &str) -> String {
    if url.contains("per_page=") {
        url.to_string()
    } else if url.contains('?') {
        format!("{url}&per_page=100")
    } else {
        format!("{url}?per_page=100")
    }
}

/// Extract the rel="next" URL from a Link header.
fn next_link(resp: &Response) -> Option<String> {
    let header = resp.headers().get(LINK)?.to_str().ok()?;
    parse_next_link(header)
}

fn parse_next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        rel.contains("rel=\"next\"").then(|| {
            url.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
    })
}

/// Percent-encode a repository path or query value, keeping its slashes readable.
pub fn encode_path(s: &str) -> String {
    encode(s, true)
}

/// Percent-encode text for use as a single URL path segment, slashes included.
pub fn encode_segment(s: &str) -> String {
    encode(s, false)
}

fn encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'.' | b'_' | b'~')
            || (keep_slash && b == b'/')
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Whether a check run, job, step, or workflow run conclusion counts as failed. REST spells
/// conclusions in lowercase and GraphQL in uppercase, so either is accepted.
pub fn is_failed(conclusion: Option<&str>) -> bool {
    conclusion.is_some_and(|c| {
        ["failure", "cancelled", "timed_out", "startup_failure"]
            .iter()
            .any(|f| c.eq_ignore_ascii_case(f))
    })
}

pub fn get_github_token(host: &str) -> Result<String> {
    // Try gh auth token first.
    if let Ok(output) = Command::new("gh")
        .args(["auth", "token", "--hostname", host])
        .output()
        && output.status.success()
    {
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }

    // Fall back to environment variables.
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(token) = std::env::var(var)
            && !token.is_empty()
        {
            return Ok(token);
        }
    }

    bail!("No GitHub token found. Run `gh auth login` or set GH_TOKEN.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_keeps_or_escapes_slashes() {
        assert_eq!(encode_path("a b/c#d.rs"), "a%20b/c%23d.rs");
        assert_eq!(encode_path("nathan/dev/fix#2"), "nathan/dev/fix%232");
        assert_eq!(encode_segment("ui/ux"), "ui%2Fux");
        assert_eq!(encode_segment("build system"), "build%20system");
        assert_eq!(encode_segment("type: bug"), "type%3A%20bug");
        assert_eq!(encode_segment("naïve"), "na%C3%AFve");
    }

    #[test]
    fn failed_conclusions_in_either_case() {
        assert!(is_failed(Some("failure")));
        assert!(is_failed(Some("timed_out")));
        assert!(is_failed(Some("STARTUP_FAILURE")));
        assert!(is_failed(Some("CANCELLED")));
        assert!(!is_failed(Some("success")));
        assert!(!is_failed(Some("ACTION_REQUIRED")));
        assert!(!is_failed(None));
    }

    #[test]
    fn next_link_found_among_rels() {
        let header = r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=5>; rel="last""#;
        assert_eq!(
            parse_next_link(header).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
    }

    #[test]
    fn next_link_absent_on_last_page() {
        let header = r#"<https://api.github.com/x?page=1>; rel="first", <https://api.github.com/x?page=4>; rel="prev""#;
        assert_eq!(parse_next_link(header), None);
    }

    #[test]
    fn per_page_appended_once() {
        assert_eq!(with_per_page("https://a/x"), "https://a/x?per_page=100");
        assert_eq!(
            with_per_page("https://a/x?state=open"),
            "https://a/x?state=open&per_page=100"
        );
        assert_eq!(
            with_per_page("https://a/x?per_page=5"),
            "https://a/x?per_page=5"
        );
    }
}
