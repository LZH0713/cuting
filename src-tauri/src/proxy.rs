//! How translation traffic reaches the network.
//!
//! "System" follows the proxy configured in macOS System Settings. A proxy
//! tool in TUN mode leaves that at "off" on purpose — it captures traffic
//! below the app — so pointing Cuting at such a tool's local port changes
//! nothing: the request takes the same route either way. A manual proxy is
//! for the other setups: an HTTP proxy the system doesn't know about, or a
//! second proxy with its own exit.

use std::error::Error as _;
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum ProxyConfig {
    /// Follow the proxy configured in System Settings.
    #[default]
    System,
    /// Connect directly and ignore any system proxy.
    Direct,
    /// Send every request through this proxy: `http://host:port`,
    /// `https://host:port`, `socks5://host:port` or `socks5h://host:port`.
    Manual { url: String },
}

impl ProxyConfig {
    /// Reject what could never work before it reaches the settings file.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Manual { url } if url.trim().is_empty() => Err("请填写代理地址".to_string()),
            // reqwest itself would quietly accept "127.0.0.1:7897" and treat
            // it as http, so the URL is checked strictly here instead.
            Self::Manual { url } => check_url(url),
            _ => Ok(()),
        }
    }
}

/// A proxy URL with a supported scheme and an explicit host and port.
fn check_url(raw: &str) -> Result<(), String> {
    let trimmed = raw.trim();
    let reject = || {
        format!(
            "代理地址「{trimmed}」无法识别：需要以 http:// 、https:// 或 socks5:// 开头，\
             并带上主机和端口，例如 http://127.0.0.1:7897"
        )
    };
    let parsed = reqwest::Url::parse(trimmed).map_err(|_| reject())?;
    let supported = matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h");
    let has_host = parsed.host_str().is_some_and(|host| !host.is_empty());
    if !supported || !has_host || parsed.port().is_none() {
        return Err(reject());
    }
    Ok(())
}

fn build_proxy(url: &str) -> Result<reqwest::Proxy, reqwest::Error> {
    reqwest::Proxy::all(url.trim())
}

/// Ports that popular proxy tools listen on by default. Used only as a hint.
const COMMON_PORTS: [u16; 14] = [7890, 7897, 7891, 1080, 1086, 1087, 6152, 6153, 2080, 8080, 8118, 10808, 10809, 20170];

/// Local ports with something listening on them — probably a proxy.
pub fn detect_local_proxies() -> Vec<u16> {
    let addr = |port: u16| std::net::SocketAddr::from(([127, 0, 0, 1], port));
    COMMON_PORTS
        .iter()
        .copied()
        .filter(|port| TcpStream::connect_timeout(&addr(*port), Duration::from_millis(200)).is_ok())
        .collect()
}

static CLIENT: Mutex<Option<(String, reqwest::Client)>> = Mutex::new(None);

/// The HTTP client for `config`. Building one sets up a TLS connector, so the
/// most recently built client per configuration is kept and reused.
pub fn client(config: &ProxyConfig) -> Result<reqwest::Client, String> {
    config.validate()?;
    let key = serde_json::to_string(config).unwrap_or_default();
    let mut cache = crate::util::lock(&CLIENT);
    if let Some((cached, client)) = cache.as_ref() {
        if cached == &key {
            return Ok(client.clone());
        }
    }
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("cuting/", env!("CARGO_PKG_VERSION")));
    match config {
        ProxyConfig::System => {}
        ProxyConfig::Direct => builder = builder.no_proxy(),
        ProxyConfig::Manual { url } => {
            // The manual proxy replaces the system one; mixing both would
            // make "which proxy did my request use" unanswerable.
            builder = builder.no_proxy().proxy(build_proxy(url).map_err(|e| e.to_string())?);
        }
    }
    let client = builder.build().map_err(|e| format!("无法创建网络客户端：{e}"))?;
    *cache = Some((key, client.clone()));
    Ok(client)
}

/// What a connection test found.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestReport {
    /// Some ordinary HTTPS site answered through this configuration.
    pub network_ok: bool,
    pub network_ms: Option<u128>,
    /// The Google endpoint answered. It is the zero-config translation
    /// service, and the one that is rate-limited by network.
    pub google_ok: bool,
    pub google_ms: Option<u128>,
    /// One or two lines, written for the settings window.
    pub summary: String,
}

/// Try the configuration exactly the way a translation would use it.
pub async fn test(config: &ProxyConfig) -> Result<TestReport, String> {
    let client = client(config)?;
    let via_proxy = matches!(config, ProxyConfig::Manual { .. });
    let mut report = TestReport {
        network_ok: false,
        network_ms: None,
        google_ok: false,
        google_ms: None,
        summary: String::new(),
    };

    // An ordinary site first: it tells "the proxy is unreachable" apart from
    // "Google refuses", which need opposite responses from the user.
    let started = Instant::now();
    let network = client.get("https://www.baidu.com/").timeout(Duration::from_secs(10)).send().await;
    report.network_ms = Some(started.elapsed().as_millis());
    if let Err(e) = network {
        report.summary = if via_proxy && e.is_connect() {
            format!("连不上代理，请检查地址和端口是否正确、代理程序是否在运行。")
        } else {
            format!("连不上网络：{}", short_error(&e))
        };
        return Ok(report);
    }
    report.network_ok = true;

    let started = Instant::now();
    let google = crate::translate::google_probe(&client).await;
    report.google_ms = Some(started.elapsed().as_millis());
    let ms = report.google_ms.unwrap_or_default();
    let what = if via_proxy { "代理可用" } else { "网络正常" };
    match google {
        Ok(_) => {
            report.google_ok = true;
            report.summary = format!("{what}，Google 翻译正常（{ms} ms）。");
        }
        Err(e) => report.summary = format!("{what}，但 Google 翻译用不了（{ms} ms）：{e}"),
    }
    Ok(report)
}

/// reqwest's own message for a failed request, without the URL noise.
fn short_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "连接超时".to_string()
    } else if let Some(source) = e.source() {
        source.to_string()
    } else {
        e.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_and_defaults() {
        let old: ProxyConfig = serde_json::from_str(r#"{"mode":"system"}"#).unwrap();
        assert_eq!(old, ProxyConfig::System);
        let manual = ProxyConfig::Manual { url: " http://127.0.0.1:7897 ".into() };
        let text = serde_json::to_string(&manual).unwrap();
        assert_eq!(text, r#"{"mode":"manual","url":" http://127.0.0.1:7897 "}"#);
        assert_eq!(serde_json::from_str::<ProxyConfig>(&text).unwrap(), manual);
        assert_eq!(serde_json::to_string(&ProxyConfig::Direct).unwrap(), r#"{"mode":"direct"}"#);
    }

    #[test]
    fn manual_urls_are_checked_before_use() {
        for url in ["http://127.0.0.1:7897", "https://proxy.local:8443", "socks5://127.0.0.1:1080", "socks5h://127.0.0.1:1080"] {
            assert!(ProxyConfig::Manual { url: url.into() }.validate().is_ok(), "{url}");
        }
        for url in ["", "   ", "127.0.0.1:7897", "7897", "ftp://127.0.0.1:21", "http://", "http://proxy.local", "nonsense"] {
            let err = ProxyConfig::Manual { url: url.into() }.validate().unwrap_err();
            assert!(err.contains("无法识别") || err.contains("请填写"), "{url:?} → {err}");
        }
        assert!(ProxyConfig::System.validate().is_ok());
        assert!(ProxyConfig::Direct.validate().is_ok());
    }

    /// The three modes must build; a bad manual URL must fail up front rather
    /// than at the first translation.
    #[test]
    fn clients_build_per_mode() {
        assert!(client(&ProxyConfig::System).is_ok());
        assert!(client(&ProxyConfig::Direct).is_ok());
        assert!(client(&ProxyConfig::Manual { url: "http://127.0.0.1:7897".into() }).is_ok());
        assert!(client(&ProxyConfig::Manual { url: "127.0.0.1:7897".into() }).is_err());
        // The strict check is what stops the scheme-less form; reqwest alone does not.
        assert!(build_proxy("127.0.0.1:7897").is_ok());
    }
}
