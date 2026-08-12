# IPTV Proxy v2 — 生产级高性能透明代理（axum + reqwest，无 Pingora）

基于 **axum + reqwest** 的 IPTV 透明代理，专为 OpenWrt IPQ60xx (aarch64) 优化。
**完全重写自旧版 Pingora 实现**：旧版在播放中出现的"没画面 / 播放一段后中断 / 需多次点击才能播"
等问题，源于 Pingora `ProxyHttp` 的响应体流式改写与上游连接复用，难以根治；v2 改为更可控的栈，
从设计上消除这些根因。

## ⚡ 与旧版的核心区别（为什么重写）

| 问题（旧版 Pingora） | v2 的解决方式 |
|------|--------|
| 播放 1 秒后中断（m3u8 分片被漏改/半截 URL） | m3u8 **整段缓冲后一次性改写**（几 KB，零代价），彻底消灭 chunk 边界截断 |
| 没画面（TS 段被 body filter 篡改） | TS/媒体段**零拷贝透明流式转发**，原样透传 `Content-Type`/`Content-Length` |
| 多次点击才能播（边缘抖动 / 连接池 stale） | 自研 **IPv4 优先 + 仅全局地址** DNS 解析器注入 reqwest，由 hyper 自动多 A 记录 failover |
| 与上游 TLS/SNI 耦合 | reqwest 原生连接池 + 自动 keep-alive，稳定复用 |

## 🎯 功能特性

- ✅ **统一 `/?url=<编码目标>` 入口**：任意 IPTV 源（m3u8 自动改写，分片也走本代理）
- ✅ **自动 m3u8 改写**：绝对 URL 与相对媒体路径统一改写为代理地址
- ✅ **`#EXT-X-KEY` / `#EXT-X-MAP` / `#EXT-X-MEDIA` 引号内 URI 改写**（密钥、fMP4 初始化段也走代理）
- ✅ **智能 Referer / UA 注入**：缺则注入默认值（均可环境变量覆盖）
- ✅ **透明 TS 代理**：媒体段零拷贝转发
- ✅ **3xx 跳转改写**：`Location` 改写为本代理 URL
- ✅ **jpeg 伪装修正**：`.ts` 被上游改名 `.jpeg` 下发时自动修正
- ✅ **SSRF 防护（默认安全）**：拦截私有/回环/链路本地/保留地址，可选域名白名单
- ✅ **HTTP/HTTPS 上游**：rustls 静态链接，无 openssl 依赖

## 📦 构建

```bash
# 本机开发（需 Rust 工具链）
cargo build --release

# 交叉编译到 OpenWrt IPQ60xx（aarch64 musl 静态链接）
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
aarch64-linux-musl-strip target/aarch64-unknown-linux-musl/release/iptv-proxy
```

> CI（`.github/workflows/build-openwrt-ipq60xx.yml`）使用 `messense/rust-musl-cross` 容器自动出包，
> 产物为静态链接 musl 二进制，可直接上传路由器。

## 🎬 使用方法

### URL 格式（统一 `/?url=` 模式）

所有目标地址通过 `url` 查询参数传入（需要 URL 编码）：

```bash
# 任意 IPTV 源（m3u8 自动改写，分片也走本代理）
curl "http://192.168.1.1:8080/?url=http%3A%2F%2F116.199.7.27%3A8006%2F00000000%2Fxxx%2Findex.m3u8"

# 播放器里填：
# http://192.168.1.1:8080/?url=http%3A%2F%2F116.199.7.27%3A8006%2Fxxx%2Findex.m3u8
```

URL 编码生成：`python3 -c "import urllib.parse;print(urllib.parse.quote('http://116.199.7.27:8006/xxx/index.m3u8',safe=''))"`

### m3u8 改写示例

**原始**（上游返回）：
```m3u8
#EXTINF:10.0,
http://116.199.4.228:8114/LIVES/segment001.ts
#EXTINF:10.0,
12345.ts
```

**代理后**（本机返回，所有分片被改写为代理地址）：
```m3u8
#EXTINF:10.0,
http://192.168.1.1:8080/?url=http%3A%2F%2F116.199.4.228%3A8114%2FLIVES%2Fsegment001.ts
#EXTINF:10.0,
http://192.168.1.1:8080/?url=http%3A%2F%2F116.199.4.228%3A8114%2FLIVES%2F12345.ts
```

## 🔧 配置（环境变量）

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `LOCAL_IP` | `192.168.1.3` | 本机 IP（用于 m3u8 改写基址），也可用 `-Li <ip>` 参数 |
| `BIND_ADDR` | `0.0.0.0:8080` | 监听地址（支持 IPv6 `[::1]:8080`） |
| `WORKERS` | CPU 核数 | tokio worker 线程数 |
| `RUST_LOG` | `info` | 日志级别：`error`/`warn`/`info`/`debug`（热路径为 debug） |
| `LOG_FILE` | `<临时目录>/iptv-proxy.log` | 日志文件路径（同时输出 stderr） |
| `ALLOW_HOSTS` | 未设置=开放 | 上游域名白名单（逗号分隔）；设置后进入白名单模式（非列表域名返回 403），`surrit.com` 始终放行 |
| `ALLOW_PRIVATE` | `0`/`false` | 设为 `1`/`true` 允许代理私有/内网 IP（SSRF 逃生开关，默认挡掉） |
| `DEFAULT_REFERER` | `https://missav.ws/dm242/cn` | 缺 Referer 时注入的值（可覆盖，修复不可配置隐患） |
| `DEFAULT_USER_AGENT` | Chrome UA | 缺 UA 时注入的值（可覆盖） |
| `PROXY_BASE` | `http://<LOCAL_IP>:<port>/?url=` | 代理基址覆盖（支持域名 / HTTPS 访问场景） |

## 🔒 安全说明

- **默认拦截 SSRF**：上游 host 若为 IP 字面量且非全局可达（私有/回环/链路本地/保留），或域名解析到此类地址，一律拒绝。
- 仅当设置 `ALLOW_HOSTS` 时进入**域名白名单**模式；未设置则保持开放（仅做 host 合法性 + 私有地址校验）。
- **内网使用**：仅绑定内网接口，用防火墙限制 8080 端口访问来源。

## 📊 监控

```bash
# 健康检查
curl "http://192.168.1.1:8080/health"   # 返回 OK

# 连接数
watch -n 1 'netstat -an | grep :8080 | wc -l'

# 日志
tail -f /tmp/iptv-proxy.log
```

## 🐛 故障排查

- **404 / 400**：确认 URL 格式为 `/?url=<编码后的目标>`（旧版 `/iptv/http://...` 格式已废弃）。
- **播放 1 秒中断**：`RUST_LOG=debug` 重启，查看日志确认 m3u8 中每个分片都被改写为 `/?url=` 形式。
- **上游 TLS 失败**：检查 `DEFAULT_REFERER` / `DEFAULT_USER_AGENT` 是否被上游防盗链要求。

## 📄 许可证

MIT License
