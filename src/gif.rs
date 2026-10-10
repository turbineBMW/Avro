//! Keyless GIF search over DuckDuckGo's image search (`f=type:gif`).
//!
//! DDG's image endpoint is undocumented: a page load yields a `vqd` token that unlocks `/i.js`
//! JSON results. The token is cached here and refreshed on the first failure. No API key,
//! no account — the trade is that a DDG-side change can break this without notice.
use anyhow::{Context, Result, anyhow};
use std::sync::{Mutex, OnceLock};

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";
/// Refuse GIFs larger than this — MMS carriers reject big attachments anyway.
pub const MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, serde::Deserialize)]
pub struct Gif {
    /// Direct URL to the animated GIF.
    #[serde(rename = "image")]
    pub url: String,
    /// Small static preview, good for a picker grid.
    pub thumbnail: String,
}

#[derive(serde::Deserialize)]
struct Page { results: Vec<Gif> }

fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| reqwest::Client::builder().user_agent(UA).timeout(std::time::Duration::from_secs(20)).build().expect("gif http client"))
}

fn vqd_cache() -> &'static Mutex<Option<String>> {
    static V: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    V.get_or_init(|| Mutex::new(None))
}

async fn fetch_vqd(query: &str) -> Result<String> {
    let html = http().get("https://duckduckgo.com/").query(&[("q", query), ("iax", "images"), ("ia", "images")]).send().await?.error_for_status()?.text().await?;
    let start = html.find("vqd=").ok_or_else(|| anyhow!("no vqd token in DDG page"))? + 4;
    // Appears both as `vqd="4-…"` and `vqd=4-…`; accept either.
    let tok: String = html[start..].trim_start_matches(['"', '\'']).chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    if tok.is_empty() { return Err(anyhow!("empty vqd token")); }
    Ok(tok)
}

async fn vqd(query: &str, refresh: bool) -> Result<String> {
    if !refresh && let Some(v) = vqd_cache().lock().unwrap().clone() { return Ok(v); }
    let v = fetch_vqd(query).await?;
    *vqd_cache().lock().unwrap() = Some(v.clone());
    Ok(v)
}

/// Search GIFs; `page` is zero-based (DDG hands back ~50–100 per page).
pub async fn search(query: &str, page: u32) -> Result<Vec<Gif>> {
    let query = query.trim();
    if query.is_empty() { return Ok(vec![]); }
    let mut refresh = false;
    for attempt in 0..2 {
        let tok = vqd(query, refresh).await?;
        let resp = http().get("https://duckduckgo.com/i.js")
            .header("Referer", "https://duckduckgo.com/")
            .query(&[("l", "us-en"), ("o", "json"), ("q", query), ("vqd", &tok), ("f", "type:gif"), ("p", "1"), ("s", &(page * 100).to_string())])
            .send().await?;
        if resp.status().is_success() {
            let page: Page = resp.json().await.context("DDG results JSON")?;
            return Ok(page.results.into_iter().filter(|g| g.url.starts_with("http")).collect());
        }
        // 403 means the token went stale (or we're rate-limited); one refresh, then give up.
        tracing::debug!(status = %resp.status(), attempt, "DDG image search rejected");
        refresh = true;
    }
    Err(anyhow!("DuckDuckGo refused the search (rate-limited?) — try again in a moment"))
}

/// Download a GIF's bytes, checking that it actually is one.
pub async fn download(url: &str) -> Result<Vec<u8>> {
    let resp = http().get(url).send().await?.error_for_status()?;
    if let Some(len) = resp.content_length() && len as usize > MAX_BYTES { return Err(anyhow!("GIF is too large to send ({} MB)", len / 1024 / 1024)); }
    let bytes = resp.bytes().await?;
    if bytes.len() > MAX_BYTES { return Err(anyhow!("GIF is too large to send")); }
    if !bytes.starts_with(b"GIF8") { return Err(anyhow!("that link is not a GIF")); }
    Ok(bytes.to_vec())
}

/// Fetch a preview thumbnail (any image format).
pub async fn thumbnail(url: &str) -> Result<Vec<u8>> {
    Ok(http().get(url).send().await?.error_for_status()?.bytes().await?.to_vec())
}
