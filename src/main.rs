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
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const DEFAULT_REFERER: &str = "https://missav.ws/dm242/cn";
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// 上游 DNS 解析缓存：强制优先 IPv4，避免家庭网络下连 IPv6 上游超时；
/// 缓存带 TTL（5 分钟），避免每个 .ts 分片都重复做 DNS 查询。
struct DnsCacheEntry {
    addr: SocketAddr,
    ts: Instant,
}

static DNS_CACHE: OnceLock<Mutex<HashMap<String, DnsCacheEntry>>> = OnceLock::new();
const DNS_TTL: Duration = Duration::from_secs(300);

/// 解析主机名为 SocketAddr，优先 IPv4；仅当不存在 A 记录时才回退 IPv6。
/// 返回的是裸 IP，Pingora 会用 `host` 作为 SNI/证书校验，不影响 HTTPS。
async fn resolve_upstream_addr(host: &str, port: u16) -> Result<SocketAddr> {
    // 1) 命中缓存且未过期，直接返回，零系统调用
    if let Some(cache) = DNS_CACHE.get() {
        if let Ok(guard) = cache.lock() {
            if let Some(e) = guard.get(host) {
                if e.ts.elapsed() < DNS_TTL {
                    return Ok(e.addr);
                }
            }
        }
    }

    // 2) 解析：lookup_host 同时返回 A 与 AAAA，过滤出 IPv4 优先
    let mut addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| Error::explain(ErrorType::InternalError, format!("DNS resolve {host}: {e}")))?;
    let addr = addrs
        .by_ref()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.next())
        .ok_or_else(|| Error::explain(ErrorType::InternalError, format!("no address for {host}")))?;

    // 3) 写回缓存（即便已过期也更新）
    let cache = DNS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        guard.insert(host.to_string(), DnsCacheEntry { addr, ts: Instant::now() });
    }

    Ok(addr)
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
    // 跨 chunk 字节缓冲：保证被 \n 截断的行 / 跨块的多字节 UTF-8 不会损坏
    byte_buf: Vec<u8>,
    // 等待其后续 URI 行的标签（#EXTINF 等），跨 chunk 保持
    pending_tag: Option<String>,
    // 预生成的代理前缀，避免每个 chunk 重复拼接
    proxy_base: Option<String>,
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
        }
    }
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
        for ext in MEDIA_EXTS {
            if path.ends_with(ext) {
                let file_part = &path[..path.len() - ext.len()];
                let filename = match file_part.rfind('/') {
                    Some(pos) => &file_part[pos+1..],
                    None => file_part,
                };
                if filename.is_empty() || filename.chars().all(|c| c.is_ascii_digit()) {
                    return false;
                }
                return true;
            }
        }
        false
    }

    fn tag_requires_uri(line: &str) -> bool {
        line.starts_with("#EXTINF:")
            || line.starts_with("#EXT-X-STREAM-INF:")
            || line.starts_with("#EXT-X-I-FRAME-STREAM-INF:")
    }
}

#[async_trait]
impl ProxyHttp for IptvProxy {
    type CTX = ProxyContext;

    fn new_ctx(&self) -> Self::CTX {
        ProxyContext::new()
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
            let decoded = urlencoding::decode(encoded)
                .map_err(|e| Error::explain(ErrorType::HTTPStatus(400), format!("decode: {e}")))?;
            let decoded_str = decoded.to_string();
            debug!("Decoded URL: {}", decoded_str);

            let mut url = url::Url::parse(&decoded_str)
                .map_err(|e| Error::explain(ErrorType::HTTPStatus(400), format!("invalid URL: {e}")))?;

            // 校验目标 host：必须存在且为合法 FQDN/IP（含 "."、且不以 "." 结尾），
            // 拒绝裸主机名（如 "sur"）或畸形主机名（如 "surrit."），
            // 既避免无谓的 DNS 查询与 500 错误刷屏，也挡掉扫描器的垃圾请求。
            let host = url.host_str().unwrap_or("").to_string();
            if host.is_empty() || !host.contains('.') || host.ends_with('.') {
                return Err(Error::explain(ErrorType::HTTPStatus(400), "invalid target host"));
            }

            // 可选上游白名单（SSRF 防护）：ALLOW_HOSTS=surrit.com,fourhoi.com
            // 设置后仅放行列表内域名（含其子域）；未设置则保持开放行为。
            if let Some(allow_list) = std::env::var("ALLOW_HOSTS").ok() {
                let allowed: Vec<String> = allow_list
                    .split(',')
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !allowed.is_empty() {
                    let host_l = host.to_lowercase();
                    let permitted = allowed
                        .iter()
                        .any(|a| host_l == *a || host_l.ends_with(&format!(".{}", a)));
                    if !permitted {
                        return Err(Error::explain(ErrorType::HTTPStatus(403), "host not allowed by ALLOW_HOSTS"));
                    }
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
            ctx.proxy_base = Some(format!(
                "http://{}:{}/?url=",
                self.config.local_ip, self.config.bind_port
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

        Err(Error::explain(ErrorType::HTTPStatus(400), "Use /?url=<encoded_target>"))
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
        // 上游连接调优：
        // - idle_timeout 拉长：让建好的 TLS 连接在连接池里多活一会儿，减少 web 播放器
        //   频繁 abort / 并发取段时反复做 TLS 握手（~0.5-1s）造成的卡顿。
        // - connection_timeout / total_connection_timeout 缩短：上游慢时快速失败，
        //   而不是让客户端（web 播放器）一直干等后超时重连，放大卡顿。
        peer.options.connection_timeout = Some(Duration::from_secs(3));
        peer.options.total_connection_timeout = Some(Duration::from_secs(8));
        peer.options.idle_timeout = Some(Duration::from_secs(60));
        peer.options.read_timeout = Some(Duration::from_secs(20));

        debug!("Upstream: {} ({}) TLS: {}", addr, host, is_https);
        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(&self, session: &mut Session, upstream_request: &mut RequestHeader, ctx: &mut Self::CTX) -> Result<()> {
        let url = ctx.target_url.as_ref()
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "No target"))?;

        let mut path_and_query = match url.query() {
            Some(q) => format!("{}?{}", url.path(), q),
            None => url.path().to_string(),
        };
        if ctx.needs_jpeg_fix {
            path_and_query = path_and_query.replace(".ts", ".jpeg");
        }
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
            upstream_response.remove_header("Content-Length");
        }

        Ok(())
    }

    fn response_body_filter(&self, _session: &mut Session, body: &mut Option<Bytes>, end_of_stream: bool, ctx: &mut Self::CTX) -> Result<Option<Duration>> {
        if !ctx.is_m3u8 {
            return Ok(None);
        }

        // 取出本块字节并累加到跨 chunk 缓冲
        let chunk = body.take().unwrap_or_default();
        ctx.byte_buf.extend_from_slice(&chunk);

        // 还没凑齐任何完整行且流未结束：暂存，本轮不输出
        if ctx.byte_buf.iter().rposition(|&b| b == b'\n').is_none() && !end_of_stream {
            *body = None;
            return Ok(None);
        }

        // 仅在 \n 字节边界切分（0x0A 不会出现在多字节 UTF-8 字符内部），
        // 因此"完整部分"一定是合法 UTF-8，可被安全解码；剩余尾巴留到下一个 chunk。
        let (complete, tail) = match ctx.byte_buf.iter().rposition(|&b| b == b'\n') {
            Some(pos) => {
                let end = pos + 1;
                (ctx.byte_buf[..end].to_vec(), ctx.byte_buf[end..].to_vec())
            }
            None => (std::mem::take(&mut ctx.byte_buf), Vec::new()),
        };
        ctx.byte_buf = tail;

        if complete.is_empty() {
            *body = None;
            return Ok(None);
        }

        let content = match std::str::from_utf8(&complete) {
            Ok(s) => s,
            Err(_) => {
                // 理论上不会触发（已在 \n 边界切分）；保险起见原样转发该块
                *body = Some(Bytes::from(complete));
                return Ok(None);
            }
        };

        let base = ctx.base_url.as_ref().expect("base_url missing");
        let origin_base = ctx.origin_base.as_ref().expect("origin_base missing");
        let proxy_base = ctx.proxy_base.as_ref().expect("proxy_base missing");

        let mut new_content = String::with_capacity(content.len() + 64);
        let mut pending_tag = ctx.pending_tag.take();

        for line in content.lines() {
            if line.starts_with('#') {
                if let Some(tag) = pending_tag.take() {
                    new_content.push_str(&tag);
                    new_content.push('\n');
                }
                if Self::tag_requires_uri(line) {
                    pending_tag = Some(line.to_string());
                } else {
                    new_content.push_str(line);
                    new_content.push('\n');
                }
            } else if line.is_empty() {
                if let Some(tag) = pending_tag.take() {
                    new_content.push_str(&tag);
                    new_content.push('\n');
                }
                new_content.push('\n');
            } else if line.starts_with("http://") || line.starts_with("https://") {
                if let Some(tag) = pending_tag.take() {
                    new_content.push_str(&tag);
                    new_content.push('\n');
                }
                new_content.push_str(proxy_base);
                new_content.push_str(&urlencoding::encode(line));
                new_content.push('\n');
            } else if Self::is_likely_media_resource(line) {
                if let Some(tag) = pending_tag.take() {
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
                pending_tag = None;
            }
        }

        // 流结束时若仍有悬挂的标签（异常 m3u8），照原行为将其刷出；否则留给后续 chunk
        if end_of_stream {
            if let Some(tag) = pending_tag.take() {
                new_content.push_str(&tag);
                new_content.push('\n');
            }
        } else {
            ctx.pending_tag = pending_tag;
        }

        debug!("M3U8 rewritten ({} bytes)", new_content.len());
        *body = Some(Bytes::from(new_content));
        Ok(None)
    }

    async fn logging(&self, session: &mut Session, e: Option<&Error>, _ctx: &mut Self::CTX) {
        let req = session.req_header();
        let status = session.response_written().map(|r| r.status.as_u16()).unwrap_or(0);
        let client = session.client_addr().map(|a| a.to_string()).unwrap_or_else(|| "unknown".into());
        if let Some(err) = e {
            // 客户端主动断连（下游写入/读取失败、连接关闭）属正常现象（播放器停止、
            // 读完列表即关连接、或客户端超时先 RST），降级为 debug 以免刷屏并误判为故障。
            let client_abort = err.esource == ErrorSource::Downstream
                && matches!(
                    err.etype,
                    ErrorType::WriteError | ErrorType::ReadError | ErrorType::ConnectionClosed
                );
            // 4xx 是客户端问题（非法请求 / 不在白名单的扫描器），并非服务故障，降级为 debug 避免刷屏
            let client_error = matches!(err.etype, ErrorType::HTTPStatus(_));
            if client_abort || client_error {
                debug!(
                    "{} {} {} - client-side (Status:{}): {}",
                    client, req.method, req.uri.path(), status, err
                );
            } else {
                error!("{} {} {} - Status:{} Error:{:?}", client, req.method, req.uri.path(), status, err);
            }
        } else {
            debug!("{} {} {} - Status:{}", client, req.method, req.uri.path(), status);
        }
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
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
    let bind_port: u16 = bind_addr.split(':').nth(1).and_then(|p| p.parse().ok()).unwrap_or(8080);

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
