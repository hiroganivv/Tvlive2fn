use async_trait::async_trait;
use bytes::Bytes;
use http::Uri;
use log::{debug, error, info};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::prelude::*;
use pingora::proxy::http_proxy_service;
use pingora::proxy::{ProxyHttp, Session};
use pingora::server::configuration::Opt;
use pingora::server::Server;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::sync::{RwLock, OnceLock};
use std::time::{Duration, Instant};

const DEFAULT_REFERER: &str = "https://missav.ws/dm242/cn";
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// 代码内置默认放行域名（含其子域），始终允许；与 ALLOW_HOSTS 环境变量取并集，
/// 确保即便设置了上游白名单，surrit.com 这类核心源站也不会被误挡。
const DEFAULT_ALLOW_HOSTS: &[&str] = &["surrit.com"];

/// 上游 DNS 解析缓存：强制优先 IPv4，避免家庭网络下连 IPv6 上游超时。
/// 策略：缓存该域名解析出的"全部 A 记录"，但每个 host 固定 sticky 使用其中某一个 IP
/// （`idx`），而非每请求都换 IP。原因：Pingora 按 (IP, SNI) 维护上游连接池，
/// sticky 能让同一频道的连续分片复用同一条已建好的 TLS 连接（idle_timeout 60s），
/// 省掉反复握手（~0.5-1s）带来的卡顿——这对单路顺序取段的播放器体验至关重要。
/// 仅当该 IP "建连失败"时才在 fail_to_connect 里把 idx 切到下一个边缘（failover），
/// 且 TTL 缩短为 60s，使持续变差的边缘能被较快轮换掉。
struct DnsCacheEntry {
    addrs: Vec<SocketAddr>,
    idx: usize,
    ts: Instant,
}

static DNS_CACHE: OnceLock<RwLock<HashMap<String, DnsCacheEntry>>> = OnceLock::new();
const DNS_TTL: Duration = Duration::from_secs(60);

/// 解析主机名为 SocketAddr，优先 IPv4（仅当无 A 记录才回退 AAAA）。
/// 返回的是裸 IP，Pingora 会用 `host` 作为 SNI/证书校验，不影响 HTTPS。
/// 命中缓存时返回当前 sticky 的 IP；未命中则解析全部 A 记录后取第一个。
async fn resolve_upstream_addr(host: &str, port: u16) -> Result<SocketAddr> {
    // 1) 命中缓存且未过期：返回当前 sticky 的 IP，零系统调用，且保证连接池可复用
    if let Some(cache) = DNS_CACHE.get() {
        if let Ok(guard) = cache.read() {
            if let Some(e) = guard.get(host) {
                if e.ts.elapsed() < DNS_TTL && !e.addrs.is_empty() {
                    return Ok(e.addrs[e.idx % e.addrs.len()]);
                }
            }
        }
    }

    // 2) 解析：lookup_host 同时返回 A 与 AAAA，收集全部 IPv4 优先
    let addrs_iter = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| Error::explain(ErrorType::InternalError, format!("DNS resolve {host}: {e}")))?;
    let mut ipv4: Vec<SocketAddr> = Vec::new();
    let mut ipv6: Vec<SocketAddr> = Vec::new();
    for a in addrs_iter {
        if a.is_ipv4() {
            ipv4.push(a);
        } else {
            ipv6.push(a);
        }
    }
    let addrs: Vec<SocketAddr> = if !ipv4.is_empty() { ipv4 } else { ipv6 };
    if addrs.is_empty() {
        return Err(Error::explain(ErrorType::InternalError, format!("no address for {host}")));
    }

    // 3) 写回缓存（即便已过期也更新），idx=0 表示先用第一个边缘
    let cache = DNS_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    if let Ok(mut guard) = cache.write() {
        guard.insert(host.to_string(), DnsCacheEntry { addrs: addrs.clone(), idx: 0, ts: Instant::now() });
    }
    Ok(addrs[0])
}

#[derive(Clone)]
pub struct ProxyConfig {
    pub local_ip: String,
    pub bind_port: u16,
}

impl ProxyConfig {
    pub fn new(local_ip: String, bind_port: u16) -> Self {
        Self { local_ip, bind_port }
    }
}

pub struct ProxyContext {
    target_url: Option<url::Url>,
    is_m3u8: bool,
    base_url: Option<String>,
    origin_base: Option<String>,
    needs_jpeg_fix: bool,

    // 跨 chunk 缓冲
    byte_buf: Vec<u8>,

    // 等待 URI 的标签
    pending_tag: Option<String>,

    // 预生成代理前缀
    proxy_base: Option<String>,

    // 压缩响应跳过改写
    skip_rewrite: bool,

    // 连接失败重试计数
    retry_count: u8,
}

impl ProxyContext {
    fn new() -> Self {
        Self {
            target_url: None,
            is_m3u8: false,
            base_url: None,
            origin_base: None,
            needs_jpeg_fix: false,
            byte_buf: Vec::new(),
            pending_tag: None,
            proxy_base: None,
            skip_rewrite: false,
            retry_count: 0,
        }
    }
}

/// 直接回写一段简单错误响应并视作"请求已处理"，以避免 request_filter 返回 Err 时
/// Pingora 内部打印 `Fail to filter request` 的 ERROR 日志（扫描器洪水会刷屏）。
/// 写入失败（多为客户端已断开）直接忽略，返回 Ok(true)。
async fn respond_client_error(session: &mut Session, status: u16, msg: &str) -> Result<bool> {
    if let Ok(mut resp) = ResponseHeader::build(status, None) {
        let _ = resp.insert_header("Content-Type", "text/plain; charset=utf-8");
        let _ = resp.insert_header("Connection", "close");
        let _ = session.write_response_header(Box::new(resp), false).await;
        let _ = session.write_response_body(Some(Bytes::from(msg.to_string())), true).await;
    }
    Ok(true)
}

pub struct IptvProxy {
    config: ProxyConfig,
}

impl IptvProxy {
    pub fn new(config: ProxyConfig) -> Self {
        Self { config }
    }

    fn is_likely_media_resource(line: &str) -> bool {
    const MEDIA_EXTS: &[&str] = &[
        ".ts", ".m3u8", ".m3u", ".mp4", ".m4s", ".m4a",
        ".aac", ".mp3", ".ogg", ".opus", ".vtt", ".srt",
        ".jpeg", ".jpg", ".png", ".key",
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

    /// 改写 m3u8 标签行中 `URI="..."` 形式的资源引用（#EXT-X-KEY、#EXT-X-MAP、
    /// #EXT-X-MEDIA、#EXT-X-SESSION-KEY 等），使加密密钥、fMP4 初始化段等资源
    /// 也统一走本代理，否则播放器会按 m3u8 的基址去请求代理服务本身而 400。
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
            // 普通字符原样拷贝
            out.push(bytes[i] as char);
            i += 1;
        }
        out
    }

    /// 对一段"已完整切分、可安全 UTF-8 解码"的 m3u8 文本逐行改写。
    /// 返回 (改写后的文本, 新的 pending_tag)。
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
                if Self::tag_requires_uri(line) {
                    pending = Some(line.to_string());
                } else {
                    // 对含 URI="..." 的标签（#EXT-X-KEY / #EXT-X-MAP / #EXT-X-MEDIA 等）
                    // 改写引号内的资源地址，让密钥与 fMP4 初始化段也走代理
                    if line.contains("URI=\"") {
                        let rewritten = Self::rewrite_quoted_uri(line, base, origin_base, proxy_base);
                        new_content.push_str(&rewritten);
                    } else {
                        new_content.push_str(line);
                    }
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
            } else if Self::is_likely_media_resource(line) {
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
                // 未识别行（无扩展名的相对 URI、异常注释等）：原样透传，绝不静默丢弃。
                // 此前这里只清 pending_tag 不输出行，任何被判为"非媒体"的 URI（如纯数字
                // 分片名）都会从播放列表里消失，直接导致播放 1 秒后无分片可拉而中断。
                if let Some(tag) = pending.take() {
                    new_content.push_str(&tag);
                    new_content.push('\n');
                }
                new_content.push_str(line);
                new_content.push('\n');
            }
        }

        // 流结束时若仍有悬挂的标签（异常 m3u8），照原行为将其刷出；否则留给后续 chunk
        if end_of_stream {
            if let Some(tag) = pending.take() {
                new_content.push_str(&tag);
                new_content.push('\n');
            }
        }

        (new_content, pending)
    }
}

#[async_trait]
impl ProxyHttp for IptvProxy {
    type CTX = ProxyContext;

    fn new_ctx(&self) -> Self::CTX {
        ProxyContext::new()
    }

    // 上游建连失败（TCP 连不上 / 连接超时 / TLS 握手失败 / 上游提前 RST 等）时自动重试 1 次：
    // 对错误调用 Error::set_retry(true)，pingora 会重新调用 upstream_peer() 再连一次。
    // fail_to_connect 只在"与上游建连失败"时触发（esource 一定是 Upstream），因此这里
    // 不逐个匹配具体的 ErrorType 变体（避免与具体 pingora 版本拼写不一致而编译失败），
    // 直接对建连失败统一重试；代理的请求全是幂等 GET，重试安全。
    // 配合上面的 Sticky DNS：重试时把该 host 的 sticky 下标切到下一个边缘节点（failover），
    // 从而绕开恰好死掉/被墙的 Cloudflare 边缘，显著减少"播放器超时断开 → 要多次点击才能播"。
    fn fail_to_connect(
    &self,
    _session: &mut Session,
    _peer: &HttpPeer,
    ctx: &mut Self::CTX,
    mut e: Box<Error>,
) -> Box<Error> {
    if ctx.retry_count < 1 {
        // 切换 Cloudflare 边缘 IP
        if let Some(host) = ctx.target_url.as_ref().and_then(|u| u.host_str()) {
            if let Some(cache) = DNS_CACHE.get() {
                if let Ok(mut g) = cache.write() {
                    if let Some(entry) = g.get_mut(host) {
                        if entry.addrs.len() > 1 {
                            entry.idx = (entry.idx + 1) % entry.addrs.len();
                            entry.ts = Instant::now();

                            debug!(
                                "DNS failover: rotated {} to IP #{}",
                                host,
                                entry.idx
                            );
                        }
                    }
                }
            }
        }

        ctx.retry_count += 1;
        e.set_retry(true);

        info!("Upstream connect failed, retrying ({e})");
    }

    e
}

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let req = session.req_header();
        let path = req.uri.path();
        let query = req.uri.query().unwrap_or("");

        if path == "/health" || (path == "/" && query.is_empty()) {
            let resp = ResponseHeader::build(200, None)?;
            session.write_response_header(Box::new(resp), false).await?;
            session.write_response_body(Some(Bytes::from("OK")), true).await?;
            return Ok(true);
        }

        if path == "/favicon.ico" {
            let resp = ResponseHeader::build(404, None)?;
            session.write_response_header(Box::new(resp), true).await?;
            return Ok(true);
        }

        if let Some(url_param) = query.split('&').find(|p| p.starts_with("url=")) {
            let encoded = &url_param[4..];
            let decoded = match urlencoding::decode(encoded) {
                Ok(d) => d,
                Err(_) => return respond_client_error(session, 400, "Bad Request").await,
            };
            let decoded_str = decoded.to_string();
            debug!("Decoded URL: {}", decoded_str);

            let mut url = match url::Url::parse(&decoded_str) {
                Ok(u) => u,
                Err(_) => return respond_client_error(session, 400, "Bad Request").await,
            };

            // 校验目标 host：必须存在且为合法 FQDN/IP（含 "."、且不以 "." 结尾），
            // 拒绝裸主机名（如 "sur"）或畸形主机名（如 "surrit."），
            // 既避免无谓的 DNS 查询与 500 错误刷屏，也挡掉扫描器的垃圾请求。
            let host = url.host_str().unwrap_or("").to_string();
            if host.is_empty() || !host.contains('.') || host.ends_with('.') {
                return respond_client_error(session, 400, "Bad Request").await;
            }

            // 内置默认放行 + 可选环境变量白名单（SSRF 防护）：
            // DEFAULT_ALLOW_HOSTS（surrit.com 及其子域）始终允许；
            // 仅当设置了 ALLOW_HOSTS 才进入白名单模式（二者取并集，非列表域名返回 403）；
            // 未设置 ALLOW_HOSTS 时保持开放（仅做 host 合法性校验，不做域名白名单限制）。
            let mut allowed: Vec<String> = DEFAULT_ALLOW_HOSTS.iter().map(|s| s.to_lowercase()).collect();
            if let Some(allow_list) = std::env::var("ALLOW_HOSTS").ok() {
                allowed.extend(
                    allow_list
                        .split(',')
                        .map(|s| s.trim().to_lowercase())
                        .filter(|s| !s.is_empty()),
                );
                let host_l = host.to_lowercase();
                let permitted = allowed
                    .iter()
                    .any(|a| host_l == *a || host_l.ends_with(&format!(".{}", a)));
                if !permitted {
                    return respond_client_error(session, 403, "Forbidden").await;
                }
            }

            // 用 path() 判断 m3u8/m3u，避免查询参数干扰
            ctx.is_m3u8 = url.path().ends_with(".m3u8") || url.path().ends_with(".m3u");

            if url.query_pairs().any(|(k, v)| k == "real_ext" && v == "jpeg") {
                ctx.needs_jpeg_fix = true;
                let clean: Vec<_> = url
                    .query_pairs()
                    .filter(|(k, _)| k != "real_ext")
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                url.query_pairs_mut().clear();
                for (k, v) in clean {
                    url.query_pairs_mut().append_pair(&k, &v);
                }
            }

            ctx.target_url = Some(url.clone());
            // 预生成代理前缀，整个请求只拼一次
            // 预生成代理前缀
ctx.proxy_base = Some(format!(
    "http://{}:{}/?url=",
    self.config.local_ip,
    self.config.bind_port
));

            let authority = url.authority().to_string();
            ctx.origin_base = Some(format!("{}://{}", url.scheme(), authority));

            let path = url.path();
            let base_path = match path.rfind('/') {
                Some(pos) => &path[..=pos],
                None => "/",
            };
            ctx.base_url = Some(format!("{}://{}{}", url.scheme(), authority, base_path));

            let path_bytes = url.path().as_bytes().to_vec();
            session.req_header_mut().set_raw_path(&path_bytes)?;
            session.req_header_mut().insert_header("Host", &authority)?;
            return Ok(false);
        }

        respond_client_error(session, 400, "Use /?url=<encoded_target>").await
    }

    async fn upstream_peer(&self, _session: &mut Session, ctx: &mut Self::CTX) -> Result<Box<HttpPeer>> {
        let target = ctx.target_url.as_ref()
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "No target URL"))?;

        let host = target.host_str().unwrap_or("localhost").to_string();
        let port = target.port().unwrap_or(if target.scheme() == "https" { 443 } else { 80 });
        let is_https = target.scheme() == "https";

        // 解析为 IPv4 优先地址，避免连 IPv6 上游超时（家庭网络常见），并带 DNS 缓存
        let addr = resolve_upstream_addr(&host, port).await?;

        let mut peer = HttpPeer::new(addr, is_https, host.clone());
        // 上游连接调优（针对 surrit.com 这类 Cloudflare 源站的"慢边缘 / 握手慢"问题）：
        // - connection_timeout 3s：单个边缘 TCP 连不上时快速失败，触发 fail_to_connect 重试到下一个 IP；
        // - total_connection_timeout 8s：覆盖 TCP+TLS 握手总耗时，超时即换边缘；
        // - idle_timeout 60s：建好的 TLS 连接留在连接池里多活一会儿，HLS 播放器并发取段时复用，
        //   省掉反复 TLS 握手（~0.5-1s）带来的卡顿；
        // - read_timeout 30s：TS 分片可能较大 / 上游偶发慢，放宽读超时避免被误判为断流。
        // OpenWrt + Cloudflare HLS 优化
		peer.options.connection_timeout = Some(Duration::from_secs(3));       // TCP
		peer.options.total_connection_timeout = Some(Duration::from_secs(8)); // TCP+TLS
		peer.options.idle_timeout = Some(Duration::from_secs(60));            // 连接池复用
		peer.options.read_timeout = Some(Duration::from_secs(30));            // TS 分片读取

        debug!("Upstream: {} ({}) TLS: {}", addr, host, is_https);
        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(&self, session: &mut Session, upstream_request: &mut RequestHeader, ctx: &mut Self::CTX) -> Result<()> {
        let url = ctx.target_url.as_ref()
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "No target"))?;

        // 拼接上游请求的 path + query。jpeg 修正只作用于 path 中的 .ts 后缀，
        // 查询参数原样保留（此前 replace 作用于整个字符串，可能误伤 query 里的 .ts 字样）
        let path = if ctx.needs_jpeg_fix {
            url.path().replace(".ts", ".jpeg")
        } else {
            url.path().to_string()
        };
        let path_and_query = match url.query() {
            Some(q) => format!("{}?{}", path, q),
            None => path,
        };
        let uri = Uri::try_from(path_and_query)
            .map_err(|e| Error::explain(ErrorType::InternalError, format!("URI: {e}")))?;
        upstream_request.set_uri(uri);

        // 设置 Host
        upstream_request.insert_header("Host", url.authority())?;

        // 从客户端原始请求中获取关键头部，优先使用客户端的真实值
        let client_req = session.req_header();

        // User-Agent：优先客户端，否则用默认
        if let Some(ua) = client_req.headers.get("user-agent")
            .and_then(|v| v.to_str().ok()) {
            upstream_request.insert_header("User-Agent", ua)?;
        } else {
            upstream_request.insert_header("User-Agent", DEFAULT_USER_AGENT)?;
        }

        // Referer：优先客户端，否则用默认
        if let Some(ref_val) = client_req.headers.get("referer")
            .and_then(|v| v.to_str().ok()) {
            upstream_request.insert_header("Referer", ref_val)?;
        } else {
            upstream_request.insert_header("Referer", DEFAULT_REFERER)?;
        }

        // 转发其他可能需要的头
        let forward_headers = ["origin", "cookie", "authorization", "x-forwarded-for"];
        for h in &forward_headers {
            if let Some(val) = client_req.headers.get(*h) {
                if let Ok(v) = val.to_str() {
                    upstream_request.insert_header(*h, v)?;
                }
            }
        }

        upstream_request.insert_header("Accept", "*/*")?;

        // 调试日志：显示实际发往上游的关键头（级别降到 debug，避免高并发日志锁竞争）
        debug!(
            "Upstream request -> UA: {}, Referer: {}",
            upstream_request.headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("none"),
            upstream_request.headers.get("referer").and_then(|v| v.to_str().ok()).unwrap_or("none")
        );

        if ctx.is_m3u8 {
            upstream_request.remove_header("Accept-Encoding");
        }

        Ok(())
    }

    async fn response_filter(&self, _session: &mut Session, upstream_response: &mut ResponseHeader, ctx: &mut Self::CTX) -> Result<()> {
        let status = upstream_response.status;

        if status == 301 || status == 302 || status == 307 || status == 308 {
            if let Some(loc) = upstream_response.headers.get("location")
                .and_then(|v| v.to_str().ok())
            {
                let loc_str = loc.to_string();
                let resolved = if let Ok(abs_url) = url::Url::parse(&loc_str) {
                    abs_url.to_string()
                } else if let Some(base) = &ctx.target_url {
                    base.join(&loc_str).map(|u| u.to_string()).unwrap_or_else(|_| loc_str.clone())
                } else {
                    loc_str.clone()
                };
                let new_loc = format!("/?url={}", urlencoding::encode(&resolved));
                upstream_response.insert_header("Location", &new_loc)?;
                debug!("Rewritten redirect: {} -> {}", loc_str, new_loc);
            }
        }

        if ctx.needs_jpeg_fix {
            upstream_response.remove_header("Content-Type");
            upstream_response.insert_header("Content-Type", "video/mp2t")?;
            upstream_response.remove_header("Content-Disposition");
        }

        if ctx.is_m3u8 {
    // 上游可能仍返回 gzip
    let enc = upstream_response
        .headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("identity");

    if !enc.eq_ignore_ascii_case("identity") {
        ctx.skip_rewrite = true;
        return Ok(());
    }

    // 改写后长度会变化
    upstream_response.remove_header("Content-Length");
}

        Ok(())
    }

fn response_body_filter(
    &self,
    _session: &mut Session,
    body: &mut Option<Bytes>,
    end_of_stream: bool,
    ctx: &mut Self::CTX,
) -> Result<Option<Duration>> {

    if !ctx.is_m3u8 || ctx.skip_rewrite {
        return Ok(None);
    }

    let chunk = body.take().unwrap_or_default();
    ctx.byte_buf.extend_from_slice(&chunk);

    // 没有完整行且未结束
    if ctx.byte_buf.iter().rposition(|&b| b == b'\n').is_none() && !end_of_stream {
        *body = None;
        return Ok(None);
    }

    let (mut complete, tail) = match ctx.byte_buf.iter().rposition(|&b| b == b'\n') {
        Some(pos) => {
            let end = pos + 1;
            (
                ctx.byte_buf[..end].to_vec(),
                ctx.byte_buf[end..].to_vec(),
            )
        }
        None => (std::mem::take(&mut ctx.byte_buf), Vec::new()),
    };

    ctx.byte_buf = tail;

    // 修复：最后一行没有换行也不能丢
    if end_of_stream && !ctx.byte_buf.is_empty() {
        complete.extend_from_slice(&std::mem::take(&mut ctx.byte_buf));
    }

    if complete.is_empty() {
        *body = None;
        return Ok(None);
    }

    let content = match std::str::from_utf8(&complete) {
        Ok(s) => s,
        Err(_) => {
            *body = Some(Bytes::from(complete));
            return Ok(None);
        }
    };

    let base = ctx.base_url.as_ref().unwrap();
    let origin_base = ctx.origin_base.as_ref().unwrap();
    let proxy_base = ctx.proxy_base.as_ref().unwrap();

    let (new_content, new_pending) = Self::rewrite_m3u8_lines(
        content,
        base,
        origin_base,
        proxy_base,
        end_of_stream,
        ctx.pending_tag.take(),
    );

    if !end_of_stream {
        ctx.pending_tag = new_pending;
    }

    *body = Some(Bytes::from(new_content));
    Ok(None)
}

   async fn logging(
    &self,
    session: &mut Session,
    e: Option<&Error>,
    _ctx: &mut Self::CTX,
) {
    let req = session.req_header();

    let status = session
        .response_written()
        .map(|r| r.status.as_u16())
        .unwrap_or(0);

    let client = session
        .client_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|| "unknown".into());

    if let Some(err) = e {
        // 客户端主动断开（播放器切台/停止/超时）
        let client_abort = err.esource == ErrorSource::Downstream
            && matches!(
                err.etype,
                ErrorType::WriteError
                    | ErrorType::ReadError
                    | ErrorType::ConnectionClosed
            );

        // 4xx 也是客户端问题
        let client_error = matches!(err.etype, ErrorType::HTTPStatus(_));

        if client_abort || client_error {
            debug!(
                "{} {} {} - client-side (Status:{}): {}",
                client,
                req.method,
                req.uri.path(),
                status,
                err
            );
        } else {
            error!(
                "{} {} {} - Status:{} Error:{:?}",
                client,
                req.method,
                req.uri.path(),
                status,
                err
            );
        }
    } else {
        debug!(
            "{} {} {} - Status:{}",
            client,
            req.method,
            req.uri.path(),
            status
        );
    }
}
}

/// 双写日志 writer：同一行日志同时输出到 stderr 与日志文件。
/// 服务以守护方式（procd / systemd，无终端）启动时，也能在
/// <LOG_FILE 或 /tmp/iptv-proxy.log> 里实时调试。
struct DualWriter {
    file: Option<File>,
}

impl Write for DualWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // stderr 写入失败（如管道关闭）不影响服务运行，忽略即可
        let _ = io::stderr().write_all(buf);
        if let Some(f) = self.file.as_mut() {
            let _ = f.write_all(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        if let Some(f) = self.file.as_mut() {
            let _ = f.flush();
        }
        Ok(())
    }
}

fn main() {
    // 日志同时输出到 stderr 与文件：默认 <系统临时目录>/iptv-proxy.log（Linux 即 /tmp/iptv-proxy.log），
    // 可用环境变量 LOG_FILE 覆盖；文件以追加方式打开，失败时降级为仅 stderr，不影响启动。
    let log_file_path = std::env::var("LOG_FILE")
        .unwrap_or_else(|_| std::env::temp_dir().join("iptv-proxy.log").to_string_lossy().into_owned());
    let log_file = match OpenOptions::new().create(true).append(true).open(&log_file_path) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("WARN: cannot open log file {log_file_path}: {e}, stderr only");
            None
        }
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,pingora_proxy=warn"))
        .format_timestamp_millis()
        .target(env_logger::Target::Pipe(Box::new(DualWriter { file: log_file })))
        .init();

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

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    // 用 SocketAddr 解析端口，兼容 IPv6（如 [::1]:8080）
    let bind_port: u16 = bind_addr.parse::<SocketAddr>().map(|sa| sa.port()).unwrap_or(8080);

    // WORKERS 真正生效：Pingora 的 Opt 没有 threads 字段，线程数由 ServerConf::threads 控制，
    // 只能通过 YAML 配置文件注入（Opt::conf）。因此当设置了 WORKERS 时，生成一份最小配置。
    let workers = std::env::var("WORKERS").ok().and_then(|v| v.parse::<usize>().ok());
    let conf_path = if let Some(n) = workers {
        let path = std::env::temp_dir().join("iptv-proxy-conf.yaml");
        let yaml = format!("version: 1\nthreads: {}\n", n);
        if std::fs::write(&path, yaml).is_ok() {
            info!("Workers (threads): {}", n);
            Some(path)
        } else {
            error!("Failed to write worker config, falling back to CPU count");
            None
        }
    } else {
        None
    };

    info!("========================================");
    info!("IPTV Proxy (unified ?url= mode)");
    info!("========================================");
    info!("Local IP: {}", local_ip);
    info!("Bind: {}", bind_addr);
    info!("Log file: {}", log_file_path);

    let config = ProxyConfig::new(local_ip.clone(), bind_port);
    let mut server = Server::new(Some(Opt {
        upgrade: false,
        daemon: false,
        nocapture: false,
        test: false,
        conf: conf_path.map(|p| p.to_string_lossy().into_owned()),
    })).expect("Failed to create server");
    server.bootstrap();

    let mut proxy_service = http_proxy_service(&server.configuration, IptvProxy::new(config));
    proxy_service.add_tcp(&bind_addr);
    server.add_service(proxy_service);

    info!("========================================");
    info!("Server listening on: {}", bind_addr);
    info!("Usage: http://<ip>:8080/?url=<target URL>");
    info!("========================================");

    server.run_forever();
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROXY: &str = "http://192.168.1.3:8080/?url=";
    const ORIGIN: &str = "http://hls.example.com";
    const BASE: &str = "http://hls.example.com/live/";

    #[test]
    fn media_resource_detection() {
        // 纯数字文件名（IPTV 常见）必须被识别为媒体——修复前的误判会导致分片被丢弃
        assert!(IptvProxy::is_likely_media_resource("12345.ts"));
        assert!(IptvProxy::is_likely_media_resource("00000000/12345.ts"));
        assert!(IptvProxy::is_likely_media_resource("segment001.ts"));
        assert!(IptvProxy::is_likely_media_resource("subdir/file.m4s"));
        assert!(IptvProxy::is_likely_media_resource("/abs/path/file.ts"));
        assert!(IptvProxy::is_likely_media_resource("file.ts?token=abc"));
        assert!(IptvProxy::is_likely_media_resource("key.key"));
        assert!(!IptvProxy::is_likely_media_resource("file.txt"));
        assert!(!IptvProxy::is_likely_media_resource("notes.txt"));
        assert!(!IptvProxy::is_likely_media_resource("README"));
    }

    #[test]
    fn quoted_uri_rewrite() {
        // 相对路径（基于 m3u8 所在目录）
        let out = IptvProxy::rewrite_quoted_uri(
            r#"#EXT-X-KEY:METHOD=AES-128,URI="key.key""#,
            BASE, ORIGIN, PROXY,
        );
        assert!(
            out.contains("URI=\"http://192.168.1.3:8080/?url=http%3A%2F%2Fhls.example.com%2Flive%2Fkey.key\""),
            "got: {out}"
        );

        // 绝对路径（基于源站根）
        let out = IptvProxy::rewrite_quoted_uri(
            r#"#EXT-X-MAP:URI="/init.mp4""#,
            BASE, ORIGIN, PROXY,
        );
        assert!(
            out.contains("URI=\"http://192.168.1.3:8080/?url=http%3A%2F%2Fhls.example.com%2Finit.mp4\""),
            "got: {out}"
        );

        // 绝对 URL 直接编码
        let out = IptvProxy::rewrite_quoted_uri(
            r#"#EXT-X-KEY:METHOD=AES-128,URI="https://cdn.example.com/k/abc.key""#,
            BASE, ORIGIN, PROXY,
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
        let (out, pending) = IptvProxy::rewrite_m3u8_lines(input, BASE, ORIGIN, PROXY, true, None);
        assert!(pending.is_none());

        // 纯数字分片必须被改写而不是被丢弃（播放 1 秒中断的根因）
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

        // 标签顺序与数量保持
        assert_eq!(out.matches("#EXTINF:10.0,").count(), 4);
        assert!(out.starts_with("#EXTM3U\n"));
        assert!(out.contains("#EXT-X-VERSION:3\n"));
        assert!(out.contains("#EXT-X-TARGETDURATION:10\n"));
    }

    #[test]
    fn pending_tag_cross_chunk() {
        // 第一个 chunk 只有 #EXTINF，未到流结束 → 标签保留在 pending，不提前输出
        let (out, pending) = IptvProxy::rewrite_m3u8_lines("#EXTINF:10.0,\n", BASE, ORIGIN, PROXY, false, None);
        assert!(out.is_empty(), "got: {out}");
        assert_eq!(pending.as_deref(), Some("#EXTINF:10.0,"));

        // 第二个 chunk 携带分片且流结束 → 标签与分片一并输出
        let (out, pending) = IptvProxy::rewrite_m3u8_lines("12345.ts\n", BASE, ORIGIN, PROXY, true, pending);
        assert!(pending.is_none());
        assert!(out.contains("#EXTINF:10.0,\n"), "got: {out}");
        assert!(
            out.contains("http://192.168.1.3:8080/?url=http%3A%2F%2Fhls.example.com%2Flive%2F12345.ts"),
            "got: {out}"
        );
    }
}
