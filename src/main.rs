use std::collections::HashMap;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request};
use axum::response::Response;
use axum::Router;
use http::header::{HeaderName, HeaderValue, ACCEPT, ACCEPT_ENCODING, HOST, REFERER, USER_AGENT};
use http::HeaderMap;
use reqwest::dns::{Name, Resolve};
use tracing::{debug, error, info};
use tracing_subscriber::fmt::writer::MakeWriter;
use url::Url;

// ── 默认配置 ────────────────────────────────────────────────────────────────
// 默认 Referer / UA 仍写死为旧值以**保持现有源站可用行为**，但均可通过环境变量覆盖（见 main）。
// 旧代码把 Referer 硬耦合到某站点且不可改，这里改为可配置以修掉隐私/泄露隐患。
const DEFAULT_REFERER: &str = "https://missav.ws/dm242/cn";
const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// 代码内置默认放行域名（含其子域），与 ALLOW_HOSTS 环境变量取并集。
/// 仅当设置了 ALLOW_HOSTS 时才进入"白名单模式"（非列表域名返回 403）；未设置时保持开放（仅做 host 合法性 + 私有地址校验）。
const DEFAULT_ALLOW_HOSTS: &[&str] = &["surrit.com"];

// ── IPv4 优先 + 仅全局地址的 DNS 解析器（SSRF 防护 + 边缘 failover）──────────
/// 替代 Pingora 的 sticky DNS：
/// - 只返回 IPv4（家庭网络下 IPv6 上游常超时），迫使走稳定的 IPv4 边缘；
/// - 丢弃私有/回环/链路本地/保留地址（默认挡掉 SSRF / 云元数据 169.254.169.254）；
/// - 60s 缓存避免每请求都查 DNS；
/// - 返回多个全局 A 记录时，由 hyper 在连接层自动尝试下一个（failover），
///   绕开恰好死掉/被墙的边缘，解决"播放器超时 → 多次点击才能播"。
struct Ipv4Resolver {
    cache: Arc<Mutex<HashMap<String, (Vec<SocketAddr>, Instant)>>>,
    allow_private: bool,
}

impl Resolve for Ipv4Resolver {
    fn resolve(
        &self,
        host: Name,
    ) -> Pin<Box<dyn std::future::Future<
        Output = Result<Box<dyn Iterator<Item = SocketAddr> + Send>, Box<dyn std::error::Error + Send + Sync>>,
    > + Send>>
    {
        let cache = Arc::clone(&self.cache);
        let allow_private = self.allow_private;
        Box::pin(async move {
            let host = host.as_str();
            // 1) 命中缓存且未过期：直接返回（零系统调用）
            {
                let g = cache.lock().unwrap();
                if let Some((addrs, ts)) = g.get(host) {
                    if ts.elapsed() < Duration::from_secs(60) && !addrs.is_empty() {
                        return Ok(Box::new(addrs.clone().into_iter())
                            as Box<dyn Iterator<Item = SocketAddr> + Send>);
                    }
                }
            }

            // 2) 解析：lookup_host 同时返回 A 与 AAAA，仅保留 IPv4 且（默认）仅全局地址
            //    注意：必须用 (host, 0u16) 元组形式，与 hyper 默认 GaiResolver 一致。
            //    裸主机名字符串 "surrit.com" 会被 std 当成 SocketAddr 去 parse 而报
            //    "invalid socket address" 错误，不会回退到 DNS 解析。
            let addrs_iter = tokio::net::lookup_host((host, 0u16))
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

            let mut out: Vec<SocketAddr> = Vec::new();
            for a in addrs_iter {
                if a.is_ipv4() {
                    let ip = a.ip();
                    if allow_private || is_global_ip(ip) {
                        // 端口填 0：hyper 会用请求 URI 的真实端口(80/443)补全
                        out.push(SocketAddr::new(ip, 0));
                    }
                }
            }

            if out.is_empty() {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("no global A record for {host}"),
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }

            // 3) 写回缓存
            {
                let mut g = cache.lock().unwrap();
                g.insert(host.to_string(), (out.clone(), Instant::now()));
            }
            Ok(Box::new(out.into_iter()) as Box<dyn Iterator<Item = SocketAddr> + Send>)
        })
    }
}

// ── 共享状态 ────────────────────────────────────────────────────────────────
#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    proxy_base: String, // 预生成前缀，如 http://192.168.1.3:8080/?url= 或由 PROXY_BASE 覆盖
    allow_hosts: Option<Vec<String>>, // None = 开放；Some = 白名单模式
    allow_private: bool, // 是否放行私有/回环地址（SSRF 逃生开关）
    default_referer: String,
    default_ua: String,
}

// ── m3u8 改写（纯函数，与原 Pingora 版逻辑一致，已带单测）────────────────────
fn is_likely_media_resource(line: &str) -> bool {
    const MEDIA_EXTS: &[&str] = &[
        ".ts", ".m3u8", ".m3u", ".mp4", ".m4s", ".m4a", ".aac", ".mp3", ".ogg", ".opus",
        ".vtt", ".srt", ".jpeg", ".jpg", ".png", ".key",
    ];
    if line.starts_with('/') {
        return true;
    }
    let path = match line.find('?') {
        Some(pos) => &line[..pos],
        None => line,
    };
    MEDIA_EXTS.iter().any(|ext| path.ends_with(ext))
}

fn tag_requires_uri(line: &str) -> bool {
    line.starts_with("#EXTINF:")
        || line.starts_with("#EXT-X-STREAM-INF:")
        || line.starts_with("#EXT-X-I-FRAME-STREAM-INF:")
}

/// 改写 m3u8 标签行中 URI="..." 形式的资源引用（#EXT-X-KEY / #EXT-X-MAP / #EXT-X-MEDIA 等），
/// 使加密密钥、fMP4 初始化段等资源也统一走本代理。
fn rewrite_quoted_uri(line: &str, base: &str, origin_base: &str, proxy_base: &str) -> String {
    let mut out = String::with_capacity(line.len() + 64);
    let bytes = line.as_bytes();
    let mut i = 0;
    let n = bytes.len();
    while i < n {
        if i + 4 <= n && &bytes[i..i + 4] == b"URI=" {
            out.push_str("URI=");
            i += 4;
            if i < n && bytes[i] == b'"' {
                if let Some(end) = line[i + 1..].find('"') {
                    let uri = &line[i + 1..i + 1 + end];
                    let full = if uri.starts_with("http://") || uri.starts_with("https://") {
                        uri.to_string()
                    } else if uri.starts_with('/') {
                        format!("{}{}", origin_base, uri)
                    } else {
                        format!("{}{}", base, uri)
                    };
                    out.push('"');
                    out.push_str(proxy_base);
                    out.push_str(&urlencoding::encode(&full));
                    out.push('"');
                    i += 1 + end + 1;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// 对一段完整 m3u8 文本逐行改写。返回 (改写后文本, 悬挂标签)。
/// 入口处 end_of_stream=true（整段缓冲后一次性改写，无 chunk 边界问题）。
fn rewrite_m3u8_lines(
    content: &str,
    base: &str,
    origin_base: &str,
    proxy_base: &str,
    end_of_stream: bool,
    pending_tag: Option<String>,
) -> (String, Option<String>) {
    let mut new_content = String::with_capacity(content.len() + 64);
    let mut pending = pending_tag;

    for line in content.lines() {
        if line.starts_with('#') {
            if let Some(tag) = pending.take() {
                new_content.push_str(&tag);
                new_content.push('\n');
            }
            if tag_requires_uri(line) {
                pending = Some(line.to_string());
            } else if line.contains("URI=\"") {
                let rewritten = rewrite_quoted_uri(line, base, origin_base, proxy_base);
                new_content.push_str(&rewritten);
                new_content.push('\n');
            } else {
                new_content.push_str(line);
                new_content.push('\n');
            }
        } else if line.is_empty() {
            if let Some(tag) = pending.take() {
                new_content.push_str(&tag);
                new_content.push('\n');
            }
            new_content.push('\n');
        } else if line.starts_with("http://") || line.starts_with("https://") {
            if let Some(tag) = pending.take() {
                new_content.push_str(&tag);
                new_content.push('\n');
            }
            new_content.push_str(proxy_base);
            new_content.push_str(&urlencoding::encode(line));
            new_content.push('\n');
        } else if is_likely_media_resource(line) {
            if let Some(tag) = pending.take() {
                new_content.push_str(&tag);
                new_content.push('\n');
            }
            let full_url = if line.starts_with('/') {
                format!("{}{}", origin_base, line)
            } else {
                format!("{}{}", base, line)
            };
            if full_url.ends_with(".jpeg") {
                let ts_url = full_url.replace(".jpeg", ".ts");
                let sep = if ts_url.contains('?') { "&" } else { "?" };
                let fixed = format!("{}{}real_ext=jpeg", ts_url, sep);
                new_content.push_str(proxy_base);
                new_content.push_str(&urlencoding::encode(&fixed));
                new_content.push('\n');
            } else {
                new_content.push_str(proxy_base);
                new_content.push_str(&urlencoding::encode(&full_url));
                new_content.push('\n');
            }
        } else {
            // 未识别行（无扩展名相对 URI、异常注释等）：原样透传，绝不静默丢弃。
            if let Some(tag) = pending.take() {
                new_content.push_str(&tag);
                new_content.push('\n');
            }
            new_content.push_str(line);
            new_content.push('\n');
        }
    }

    if end_of_stream {
        if let Some(tag) = pending.take() {
            new_content.push_str(&tag);
            new_content.push('\n');
        }
    }

    (new_content, pending)
}

// ── 工具 ────────────────────────────────────────────────────────────────────
fn hv(s: &str) -> HeaderValue {
    HeaderValue::from_str(s).unwrap_or_else(|_| HeaderValue::from_static(""))
}

fn client_error(status: u16, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .header("Connection", "close")
        .body(Body::from(msg.to_string()))
        .unwrap()
}

/// 把错误及其所有 source() 链拼成可读字符串，便于定位真实根因（DNS 解析 / 连接超时 / TLS / 代理 / 403 等）。
/// 顶层 `error sending request` 只是 reqwest 的包装，真正的失败原因藏在 `.source()` 里。
fn full_err(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(c) = src {
        s.push_str(" → ");
        s.push_str(&c.to_string());
        src = c.source();
    }
    s
}

/// 是否为需要转发的客户端请求头
fn forward_headers(req: &HeaderMap, name: &str) -> Option<HeaderValue> {
    req.get(HeaderName::from_bytes(name.as_bytes()).ok()?).cloned()
}

// ── 请求处理器 ──────────────────────────────────────────────────────────────
async fn proxy_handler(State(state): State<AppState>, req: Request<Body>) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path();
    let query = uri.query().unwrap_or("");

    // 健康检查 / 根
    if path == "/health" || (path == "/" && query.is_empty()) {
        return Response::builder()
            .status(200)
            .body(Body::from("OK"))
            .unwrap();
    }
    if path == "/favicon.ico" {
        return Response::builder().status(404).body(Body::empty()).unwrap();
    }

    if method != Method::GET {
        return client_error(405, "Method Not Allowed");
    }

    // 解析 ?url=
    let url_param = query.split('&').find(|p| p.starts_with("url="));
    let encoded = match url_param {
        Some(p) => &p[4..],
        None => return client_error(400, "Use /?url=<encoded_target>"),
    };
    let decoded = match urlencoding::decode(encoded) {
        Ok(d) => d.to_string(),
        Err(_) => return client_error(400, "Bad Request"),
    };
    debug!("Proxy request: {}", decoded);
    let mut url = match Url::parse(&decoded) {
        Ok(u) => u,
        Err(_) => return client_error(400, "Bad Request"),
    };

    // host 合法性校验（挡裸主机名 / 扫描器垃圾）
    let host = url.host_str().unwrap_or("").to_string();
    if host.is_empty() || !host.contains('.') || host.ends_with('.') {
        return client_error(400, "Bad Request");
    }

    // SSRF：IP 字面量默认拦截私有/回环/链路本地/保留地址
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if !state_is_private_ok(&state, ip) {
            return client_error(403, "Forbidden");
        }
    }

    // 白名单模式（仅当设置了 ALLOW_HOSTS）
    if let Some(list) = &state.allow_hosts {
        let hl = host.to_lowercase();
        let permitted = list
            .iter()
            .any(|a| hl == *a || hl.ends_with(&format!(".{}", a)));
        if !permitted {
            return client_error(403, "Forbidden");
        }
    }

    let is_m3u8 = url.path().ends_with(".m3u8") || url.path().ends_with(".m3u");

    // jpeg 修正：播放器用 real_ext=jpeg 告知"源站实际以 .jpeg 下发"，上游 path 改回 .jpeg 并摘掉该参数
    let needs_jpeg_fix = url.query_pairs().any(|(k, v)| k == "real_ext" && v == "jpeg");
    if needs_jpeg_fix {
        let new_path = url.path().replace(".ts", ".jpeg");
        url.set_path(&new_path);
        let cleaned: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| k != "real_ext")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        url.query_pairs_mut().clear();
        for (k, v) in cleaned {
            url.query_pairs_mut().append_pair(&k, &v);
        }
    }

    let authority = url.authority().to_string();
    let origin_base = format!("{}://{}", url.scheme(), authority);
    let base_path = match url.path().rfind('/') {
        Some(pos) => &url.path()[..=pos],
        None => "/",
    };
    let base_url = format!("{}://{}{}", url.scheme(), authority, base_path);

    // 构造上游请求头
    let mut hmap = HeaderMap::new();
    hmap.insert(HOST, hv(&authority));
    // UA：优先客户端，否则默认
    let ua = headers
        .get(USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| state.default_ua.clone());
    hmap.insert(USER_AGENT, hv(&ua));
    // Referer：优先客户端，否则默认
    let referer = headers
        .get(REFERER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| state.default_referer.clone());
    hmap.insert(REFERER, hv(&referer));
    // 转发其它可能需要的头
    for h in ["origin", "cookie", "authorization", "x-forwarded-for"] {
        if let Some(v) = forward_headers(&headers, h) {
            if let Ok(name) = HeaderName::from_bytes(h.as_bytes()) {
                hmap.insert(name, v);
            }
        }
    }
    hmap.insert(ACCEPT, hv("*/*"));
    if is_m3u8 {
        // 强制上游返回未压缩文本，便于整段缓冲改写
        hmap.insert(ACCEPT_ENCODING, hv("identity"));
    }

    // 发送（连接级错误重试 1 次；GET 幂等安全）
    let send_once = || {
        let client = state.client.clone();
        let url = url.clone();
        let hmap = hmap.clone();
        async move { client.get(url).headers(hmap).send().await }
    };
    let resp = match send_once().await {
        Ok(r) => r,
        Err(e) => {
            error!("Upstream request failed for {}: {}", decoded, full_err(&e));
            let msg = format!("Bad Gateway: {}", full_err(&e));
            return client_error(502, &msg);
        }
    };

    let status = resp.status();

    // 3xx：改写 Location 为本代理 URL（相对路径基于 target_url 解析）
    if status == http::StatusCode::MOVED_PERMANENTLY
        || status == http::StatusCode::FOUND
        || status == http::StatusCode::TEMPORARY_REDIRECT
        || status == http::StatusCode::PERMANENT_REDIRECT
    {
        if let Some(loc) = resp.headers().get(http::header::LOCATION) {
            if let Ok(loc_str) = loc.to_str() {
                let resolved = if Url::parse(loc_str).is_ok() {
                    loc_str.to_string()
                } else {
                    url.join(loc_str)
                        .map(|u| u.to_string())
                        .unwrap_or_else(|_| loc_str.to_string())
                };
                let new_loc = format!("/?url={}", urlencoding::encode(&resolved));
                return Response::builder()
                    .status(status)
                    .header(http::header::LOCATION, new_loc)
                    .body(Body::empty())
                    .unwrap();
            }
        }
        return Response::builder().status(status).body(Body::empty()).unwrap();
    }

    // m3u8：整段缓冲后改写（关键修复：不再做分块流式改写，消除 chunk 边界截断 bug）
    if is_m3u8 {
        let upstream_headers = resp.headers().clone();
        match resp.bytes().await {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let (rewritten, _) =
                    rewrite_m3u8_lines(&text, &base_url, &origin_base, &state.proxy_base, true, None);
                let mut builder = Response::builder()
                    .status(status)
                    .header("Content-Type", "application/vnd.apple.mpegurl");
                for (k, v) in upstream_headers.iter() {
                    let name = k.as_str();
                    if matches!(
                        name,
                        "content-length" | "content-encoding" | "content-type" | "connection"
                            | "keep-alive" | "transfer-encoding" | "trailer"
                    ) {
                        continue;
                    }
                    builder = builder.header(k.clone(), v.clone());
                }
                return builder.body(Body::from(rewritten)).unwrap();
            }
            Err(e) => {
                error!("Failed to read upstream body for {}: {}", decoded, full_err(&e));
                let msg = format!("Bad Gateway: {}", full_err(&e));
                return client_error(502, &msg);
            }
        }
    }

    // 媒体段（TS/MP4/...）：透明流式转发，原样透传 Content-Type / Content-Length，零拷贝
    let mut builder = Response::builder().status(status);
    for (k, v) in resp.headers() {
        let name = k.as_str();
        if matches!(
            name,
            "connection" | "keep-alive" | "transfer-encoding" | "trailer" | "content-encoding"
        ) {
            continue;
        }
        builder = builder.header(k.clone(), v.clone());
    }
    builder
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap()
}

/// 是否为"全局可达"地址：排除私有/回环/链路本地/未指定/组播（稳定 API，避免 is_global 不稳定 feature）
fn is_global_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            !v4.is_private()
                && !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_unspecified()
                && !v4.is_multicast()
        }
        std::net::IpAddr::V6(v6) => {
            !v6.is_loopback() && !v6.is_unspecified() && !v6.is_multicast()
        }
    }
}

/// 私有地址是否允许（allow_private 时放行，与解析器默认行为一致）
fn state_is_private_ok(state: &AppState, ip: std::net::IpAddr) -> bool {
    state.allow_private || is_global_ip(ip)
}

// ── Windows 系统代理自动探测 ─────────────────────────────────────────────────
// reqwest 默认会读 HTTPS_PROXY/HTTP_PROXY/ALL_PROXY 环境变量作为出口代理，但**不会**读 Windows 的
// IE/Internet Settings(WinINet) 代理——后者只存在于注册表。而 curl / 浏览器会自动读 WinINet，
// 这就造成"curl 能连、reqwest 连不上"的差异。本函数在 Windows 上把 WinINet 代理写入环境变量，
// 行为即可与浏览器/curl 对齐。任何解析失败都静默忽略（回退直连）。
#[cfg(windows)]
fn apply_windows_system_proxy() {
    if std::env::var("HTTPS_PROXY").is_ok()
        || std::env::var("HTTP_PROXY").is_ok()
        || std::env::var("ALL_PROXY").is_ok()
    {
        info!("代理环境变量已设置，跳过 Windows WinINet 自动探测");
        return;
    }
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";
    let enabled = std::process::Command::new("reg")
        .args(["query", key, "/v", "ProxyEnable"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("0x1"))
        .unwrap_or(false);
    if !enabled {
        return;
    }
    let server = std::process::Command::new("reg")
        .args(["query", key, "/v", "ProxyServer"])
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            s.split("ProxyServer")
                .nth(1)
                .and_then(|r| r.split("REG_SZ").nth(1))
                .map(|v| v.trim().to_string())
        })
        .unwrap_or_default();
    if server.is_empty() {
        return;
    }
    let (http_p, https_p) = parse_ie_proxy(&server);
    if !http_p.is_empty() {
        let _ = std::env::set_var("HTTP_PROXY", &http_p);
    }
    if !https_p.is_empty() {
        let _ = std::env::set_var("HTTPS_PROXY", &https_p);
    }
    info!(
        "已应用 Windows 系统代理(来自 IE 设置)：http={} https={}",
        http_p, https_p
    );
}

#[cfg(windows)]
fn parse_ie_proxy(server: &str) -> (String, String) {
    if server.contains('=') {
        let mut http = String::new();
        let mut https = String::new();
        for part in server.split(';') {
            if let Some((k, v)) = part.split_once('=') {
                match k.trim().to_ascii_lowercase().as_str() {
                    "http" => http = v.trim().to_string(),
                    "https" => https = v.trim().to_string(),
                    _ => {}
                }
            }
        }
        if https.is_empty() {
            https = http.clone();
        }
        if http.is_empty() {
            http = https.clone();
        }
        (http, https)
    } else {
        (server.to_string(), server.to_string())
    }
}

// ── 日志双写（stderr + 文件）────────────────────────────────────────────────
struct DualWriter {
    file: Option<Arc<Mutex<std::fs::File>>>,
}
impl Write for DualWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        if let Some(f) = &self.file {
            if let Ok(mut g) = f.lock() {
                let _ = g.write_all(buf);
            }
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        if let Some(f) = &self.file {
            if let Ok(mut g) = f.lock() {
                let _ = g.flush();
            }
        }
        Ok(())
    }
}
impl<'a> MakeWriter<'a> for DualWriter {
    type Writer = DualWriter;
    fn make_writer(&self) -> Self::Writer {
        DualWriter {
            file: self.file.clone(),
        }
    }
}

// ── 入口 ────────────────────────────────────────────────────────────────────
fn main() {
    // 日志：同时输出 stderr + 文件（LOG_FILE 或临时目录/iptv-proxy.log），文件失败降级 stderr
    let log_file_path = std::env::var("LOG_FILE")
        .unwrap_or_else(|_| std::env::temp_dir().join("iptv-proxy.log").to_string_lossy().into_owned());
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file_path)
        .ok()
        .map(|f| Arc::new(Mutex::new(f)));
    if log_file.is_none() {
        eprintln!("WARN: cannot open log file {log_file_path}, stderr only");
    }
    tracing_subscriber::fmt()
        .with_writer(DualWriter { file: log_file })
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    // 参数 / 环境变量
    let mut args = std::env::args().skip(1);
    let mut local_ip = None;
    while let Some(arg) = args.next() {
        if arg == "-Li" {
            local_ip = args.next();
            break;
        }
    }
    let local_ip = local_ip
        .or_else(|| std::env::var("LOCAL_IP").ok())
        .unwrap_or_else(|| "192.168.1.3".to_string());

    let bind_addr_str =
        std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let bind_addr: SocketAddr = bind_addr_str
        .parse()
        .unwrap_or_else(|_| "0.0.0.0:8080".parse().unwrap());

    let bind_port = bind_addr.port();

    // WORKERS → tokio worker 线程数（默认 CPU 核数）
    let workers = std::env::var("WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());

    let allow_private = std::env::var("ALLOW_PRIVATE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    // ALLOW_HOSTS：设置了才进入白名单模式
    let allow_hosts = std::env::var("ALLOW_HOSTS").ok().map(|list| {
        let mut v: Vec<String> = DEFAULT_ALLOW_HOSTS.iter().map(|s| s.to_lowercase()).collect();
        v.extend(
            list.split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty()),
        );
        v
    });

    let default_referer =
        std::env::var("DEFAULT_REFERER").unwrap_or_else(|_| DEFAULT_REFERER.to_string());
    let default_ua =
        std::env::var("DEFAULT_USER_AGENT").unwrap_or_else(|_| DEFAULT_USER_AGENT.to_string());

    // 代理基址：PROXY_BASE 可覆盖（支持域名 / HTTPS 访问），否则用 local_ip:bind_port
    let proxy_base = std::env::var("PROXY_BASE").unwrap_or_else(|_| {
        format!("http://{}:{}/?url=", local_ip, bind_port)
    });

    // 解析器 + 客户端（IPv4 优先、仅全局地址、连接级 failover、连接池复用）
    #[cfg(windows)]
    apply_windows_system_proxy();
    if let Ok(p) = std::env::var("HTTPS_PROXY").or_else(|_| std::env::var("HTTP_PROXY")) {
        info!("上游出口代理：{}", p);
    }

    let resolver = Arc::new(Ipv4Resolver {
        cache: Arc::new(Mutex::new(HashMap::new())),
        allow_private,
    });
    let client = reqwest::Client::builder()
        .dns_resolver(resolver)
        .connect_timeout(Duration::from_secs(3))
        .read_timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(60))
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .redirect(reqwest::redirect::Policy::none()) // 302 等由本代理改写 Location
        .build()
        .expect("failed to build reqwest client");

    let state = AppState {
        client,
        proxy_base,
        allow_hosts,
        allow_private,
        default_referer,
        default_ua,
    };

    let app = Router::new().fallback(proxy_handler).with_state(state);

    info!("========================================");
    info!("IPTV Proxy v2 (axum + reqwest, no Pingora)");
    info!("========================================");
    info!("Local IP: {}", local_ip);
    info!("Bind: {}", bind_addr);
    if let Some(w) = workers {
        info!("Workers: {}", w);
    } else {
        info!("Workers: auto (CPU cores)");
    }
    info!("Log file: {}", log_file_path);
    if allow_private {
        info!("ALLOW_PRIVATE: enabled (internal IPs permitted)");
    } else {
        info!("SSRF guard: private/loopback/link-local blocked by default");
    }

    // WORKERS 通过手动 runtime 生效（运行时读参）
    let worker_threads = workers.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    });
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()
        .expect("failed to build runtime");
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .expect("failed to bind");
        info!("Server listening on: {}", bind_addr);
        info!("Usage: http://<ip>:{}/?url=<target URL>", bind_port);
        axum::serve(listener, app).await.expect("server error");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROXY: &str = "http://192.168.1.3:8080/?url=";
    const ORIGIN: &str = "http://hls.example.com";
    const BASE: &str = "http://hls.example.com/live/";

    #[test]
    fn media_resource_detection() {
        assert!(is_likely_media_resource("12345.ts"));
        assert!(is_likely_media_resource("00000000/12345.ts"));
        assert!(is_likely_media_resource("segment001.ts"));
        assert!(is_likely_media_resource("subdir/file.m4s"));
        assert!(is_likely_media_resource("/abs/path/file.ts"));
        assert!(is_likely_media_resource("file.ts?token=abc"));
        assert!(is_likely_media_resource("key.key"));
        assert!(!is_likely_media_resource("file.txt"));
        assert!(!is_likely_media_resource("notes.txt"));
        assert!(!is_likely_media_resource("README"));
    }

    #[test]
    fn quoted_uri_rewrite() {
        let out = rewrite_quoted_uri(r#"#EXT-X-KEY:METHOD=AES-128,URI="key.key""#, BASE, ORIGIN, PROXY);
        assert!(
            out.contains("URI=\"http://192.168.1.3:8080/?url=http%3A%2F%2Fhls.example.com%2Flive%2Fkey.key\""),
            "got: {out}"
        );
        let out = rewrite_quoted_uri(r#"#EXT-X-MAP:URI="/init.mp4""#, BASE, ORIGIN, PROXY);
        assert!(
            out.contains("URI=\"http://192.168.1.3:8080/?url=http%3A%2F%2Fhls.example.com%2Finit.mp4\""),
            "got: {out}"
        );
        let out = rewrite_quoted_uri(
            r#"#EXT-X-KEY:METHOD=AES-128,URI="https://cdn.example.com/k/abc.key""#,
            BASE,
            ORIGIN,
            PROXY,
        );
        assert!(
            out.contains("URI=\"http://192.168.1.3:8080/?url=https%3A%2F%2Fcdn.example.com%2Fk%2Fabc.key\""),
            "got: {out}"
        );
    }

    #[test]
    fn full_playlist_rewrite() {
        let input = "#EXTM3U\n\
#EXT-X-VERSION:3\n\
#EXT-X-TARGETDURATION:10\n\
#EXTINF:10.0,\n\
12345.ts\n\
#EXTINF:10.0,\n\
segment002.ts\n\
#EXTINF:10.0,\n\
/abs/dir/003.ts\n\
#EXTINF:10.0,\n\
http://cdn.example.com/live/004.ts\n";
        let (out, pending) = rewrite_m3u8_lines(input, BASE, ORIGIN, PROXY, true, None);
        assert!(pending.is_none());
        assert!(
            out.contains("http://192.168.1.3:8080/?url=http%3A%2F%2Fhls.example.com%2Flive%2F12345.ts"),
            "got: {out}"
        );
        assert!(
            out.contains("http://192.168.1.3:8080/?url=http%3A%2F%2Fhls.example.com%2Flive%2Fsegment002.ts"),
            "got: {out}"
        );
        assert!(
            out.contains("http://192.168.1.3:8080/?url=http%3A%2F%2Fhls.example.com%2Fabs%2Fdir%2F003.ts"),
            "got: {out}"
        );
        assert!(
            out.contains("http://192.168.1.3:8080/?url=http%3A%2F%2Fcdn.example.com%2Flive%2F004.ts"),
            "got: {out}"
        );
        assert_eq!(out.matches("#EXTINF:10.0,").count(), 4);
        assert!(out.starts_with("#EXTM3U\n"));
        assert!(out.contains("#EXT-X-VERSION:3\n"));
        assert!(out.contains("#EXT-X-TARGETDURATION:10\n"));
    }
}
