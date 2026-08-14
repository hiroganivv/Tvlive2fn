# IPTV Proxy - 基于 Pingora 的流媒体反向代理

一个用 Rust + [Cloudflare Pingora](https://github.com/cloudflare/pingora) 编写的轻量 HTTP 反向代理，专为代理 IPTV / HLS（m3u8 + TS）直播流设计。它把上游返回的播放列表（m3u8）中的媒体地址统一改写成指向本机的地址，并对请求头做必要的注入（User-Agent / Referer / Host），从而绕过源站对 Referer、Host 的校验。

> 本项目面向 OpenWrt（IPQ60xx，aarch64 1GB RAM）静态编译优化，但也可在任意 Linux 上以 systemd 运行。

---

## ⚠️ 与旧版（/iptv/、/proxy/）的差异

**本版本已重构为统一的 `/?url=` 模式。** 旧的 `/iptv/http://...` 和 `/proxy/host:port/...` 用法**已移除**，使用旧格式访问会直接返回 `400 Use /?url=<encoded_target>`。所有功能（m3u8 改写、TS 透明代理、Referer 注入）都收敛到 `/?url=<URL 编码后的目标地址>` 这一种入口。

---

## 🎯 功能特性

- ✅ **统一代理入口**：`/?url=<编码目标>`，可代理任意 HTTP/HTTPS 资源。
- ✅ **m3u8 自动改写**：响应体中的绝对（`http(s)://`）与相对媒体地址，全部改写为指向本机的 `/?url=` 形式；支持嵌套的主播放列表（`#EXT-X-STREAM-INF`、`#EXT-X-I-FRAME-STREAM-INF`）。
- ✅ **重定向继承**：上游返回的 3xx `Location` 会被改写为 `/?url=<编码后的完整地址>`，使跳转也走代理。
- ✅ **请求头注入**：自动设置 `Host`，注入 `User-Agent` 与 `Referer`（优先使用客户端真实值，缺失时回退到内置默认值）；转发 `Origin / Cookie / Authorization / X-Forwarded-For`。
- ✅ **`real_ext=jpeg` 兼容**：针对部分把 TS 切片伪装成 `.jpeg` 返回的源站，支持 `.ts` ↔ `.jpeg` 互转。
- ✅ **HTTP/HTTPS 自适应**：根据目标 URL 的 scheme 自动选择是否启用 TLS。
- ✅ **零解析负担**：m3u8 仅在检测到时才改写，普通二进制/文本响应原样透传。

### 默认值（硬编码，见 `src/main.rs`）

| 项 | 默认值 |
|----|--------|
| User-Agent（客户端缺失时） | `Mozilla/5.0 (Windows NT 10.0; Win64; x64) ... Chrome/120.0.0.0 Safari/537.36` |
| Referer（客户端缺失时） | `https://missav.ws/dm242/cn` |
| 健康检查路径 | `/health`（或 `/`） |
| 绑定地址 | `0.0.0.0:8080` |

---

## 🔧 配置

### 环境变量

| 变量 | 说明 | 默认值 |
|------|------|--------|
| `LOCAL_IP` | 本机 IP，用于把 m3u8 中的地址改写成 `http://<LOCAL_IP>:<PORT>/?url=...` | `192.168.1.1` |
| `BIND_ADDR` | 监听地址（host:port） | `0.0.0.0:8080` |
| `RUST_LOG` | 日志级别：`error`/`warn`/`info`/`debug` | `info` |

> 命令行参数 `-Li <ip>` 可覆盖 `LOCAL_IP`（例如 `iptv-proxy -Li 192.168.1.3`）。
>
> **注意**：部署脚本中设置的 `WORKERS` 环境变量在当前版本中**未被二进制读取**——工作线程数由 Pingora 按 CPU 核心数自动决定。设置 `WORKERS` 不会报错，也不影响启动。

---

## 🎬 使用方法

### URL 格式

目标地址必须是 **URL 编码** 后作为 `url` 查询参数传入：

```
http://<本机IP>:<PORT>/?url=<URL编码后的目标地址>
```

示例（以 `LOCAL_IP=192.168.1.3` 为例）：

```bash
# 代理一个 m3u8 播放列表（目标地址需编码）
curl "http://192.168.1.3:8080/?url=http%3A%2F%2F116.199.7.27%3A8006%2F00000000%2Fxxx%2Findex.m3u8"

# 代理一个 https 源
curl "http://192.168.1.3:8080/?url=https%3A%2F%2Fsurrit.com%2Fpath%2Ffile.m3u8"

# 健康检查
curl "http://192.168.1.3:8080/health"
```

返回的 m3u8 中，所有 TS 切片地址都会被改写成新的代理入口，播放器只需请求本机地址即可：

```m3u8
#EXTM3U
#EXT-X-VERSION:3
#EXTINF:10.0,
http://192.168.1.3:8080/?url=http%3A%2F%2F116.199.4.228%3A8114%2FLIVES%2Fsegment001.ts
http://192.168.1.3:8080/?url=http%3A%2F%2F116.199.4.228%3A8114%2FLIVES%2Fsegment002.ts
```

### `real_ext=jpeg` 兼容

当某个 TS 切片在源站以 `.jpeg` 形式提供时，在改写后的地址后追加 `&real_ext=jpeg`（或 `?real_ext=jpeg`）即可让代理以 `.ts` 路径向上游请求、并把返回内容当 `video/mp2t` 处理：

```bash
# 源站实际是 TS，却用 .jpeg 路径
curl "http://192.168.1.3:8080/?url=http%3A%2F%2Fexample.com%2FLIVES%2Fsegment001.jpeg%3Freal_ext%3Djpeg"
```

---

## 🔄 请求生命周期（代码视角，见 `src/main.rs`）

```
客户端请求 /?url=<encoded>
        │
        ▼
request_filter
  ├─ /health 或 /       → 直接返回 "OK" 200，短路
  ├─ /favicon.ico       → 直接返回 404，短路
  ├─ 含 url= 参数        → URL 解码 + 解析；标记 is_m3u8 / needs_jpeg_fix；
  │                       清理 real_ext 参数；写入 ctx.target_url / base_url / origin_base
  └─ 其它               → 400 "Use /?url=<encoded_target>"
        │
        ▼
upstream_peer            → 按 host:port 建连，scheme=https 时启用 TLS
        │
        ▼
upstream_request_filter  → 重建请求 URI；注入 Host / UA / Referer / 转发头；
                           needs_jpeg_fix 时把 .ts 换成 .jpeg；
                           m3u8 场景移除 Accept-Encoding（防止被压缩）
        │
        ▼
response_filter          → 3xx Location 改写为 /?url= ；
                           needs_jpeg_fix 时改 Content-Type=video/mp2t、
                           移除 Content-Disposition；
                           m3u8 场景移除 Content-Length（响应体将被改写）
        │
        ▼
response_body_filter     → m3u8 逐行改写：
                             绝对 http(s):// 行      → /?url=<encoded>
                             相对/路径媒体行          → 先解析为完整 URL 再 /?url=<encoded>
                             .jpeg 行                → 转成 .ts?real_ext=jpeg 再编码
                             #EXTINF 等标签行         → 缓冲并挂到下一行 URI 前
        │
        ▼
logging                  → 记录客户端地址、方法、路径、状态码、错误
```

---

## 📦 部署

仓库提供两个部署脚本，二者均使用当前版本的 `/?url=` 用法部署二进制，但**脚本末尾的测试 `curl` 命令仍沿用旧 `/iptv/` 格式，会返回 400**——请以本 README 的 `/?url=` 用法为准。

### 1. OpenWrt（aarch64 musl 静态编译）— `deploy-openwrt.sh`

交叉编译为 `aarch64-unknown-linux-musl`，通过 `procd` 托管，并自动写入 sysctl 网络/文件描述符调优。

```bash
# 编辑脚本中的连接信息
#   TARGET_HOST="192.168.1.3"
#   TARGET_USER="root"
#   TARGET_PASS="password"
#   LOCAL_IP="192.168.1.3"
chmod +x deploy-openwrt.sh
./deploy-openwrt.sh
```

前置工具链：

```bash
rustup target add aarch64-unknown-linux-musl
sudo apt install -y musl-tools                  # 提供 aarch64-linux-musl-gcc / -strip
sudo apt install -y sshpass                     # 用于自动上传
```

### 2. 普通 Linux（systemd）— `deploy.sh`

本机原生编译，部署为 systemd 服务，自动 `Restart=always`。注意该脚本内置了硬编码的 SSH 密码与路径，仅供个人内网使用，正式环境请改为密钥登录。

```bash
chmod +x deploy.sh
./deploy.sh
```

### 服务管理（OpenWrt）

```bash
/etc/init.d/iptv-proxy start
/etc/init.d/iptv-proxy stop
/etc/init.d/iptv-proxy restart
/etc/init.d/iptv-proxy status
logread -f | grep iptv-proxy
```

---

## 🛠 构建（手动）

```bash
# 本机运行（调试）
LOCAL_IP=127.0.0.1 RUST_LOG=debug cargo run

# 交叉编译 OpenWrt
cargo build --release --target aarch64-unknown-linux-musl
aarch64-linux-musl-strip target/aarch64-unknown-linux-musl/release/iptv-proxy
```

Pingora 以 git 提交 `d9e6d7a` 引入（已启用 `lb` + `rustls`，关闭默认特性）。`[profile.release]` 已开启 `lto = "fat"`、`codegen-units = 1`、`strip = true`、`panic = "abort"`，以最小化二进制体积。

---

## 📊 性能

基于 Pingora（异步、零拷贝 `Bytes` 缓冲、连接复用），在目标硬件（IPQ60xx，4 核 A53）上设计目标：

| 指标 | 设计目标 |
|------|----------|
| 最大并发连接 | 50,000+ |
| 单连接额外延迟 | < 10ms |
| 内存占用 | 30–50MB |
| 工作线程数 | 自动 = CPU 核心数 |

可通过 `test-performance.sh <host> <port>`（依赖 `wrk`）做并发压测与内存/连接数监控。

---

## 🔒 安全说明

- **开放转发代理**：当前实现对 `url` 目标**不做任何白名单校验**，且默认绑定 `0.0.0.0`。这意味着它能代理到任意可达地址（含本机/内网服务），属于开放代理行为。这是本项目的既定设计，请自行评估风险。
- **仅限内网**：强烈建议只在内网使用，并通过防火墙限制来源（例如仅允许 `192.168.1.0/24` 访问 8080 端口）。
- **凭据**：`deploy.sh` 内含明文 SSH 密码，请勿在不可信环境提交或运行。

---

## 🐛 故障排查

| 现象 | 排查 |
|------|------|
| 全部返回 `400 Use /?url=...` | 确认使用的是 `/?url=<编码地址>`，而非旧版 `/iptv/...` |
| 服务不启动 | `LOCAL_IP=192.168.1.3 RUST_LOG=debug /usr/bin/iptv-proxy` 看错误；检查 8080 端口占用 |
| m3u8 未被改写 | 确认目标文件后缀为 `.m3u8`/`.m3u`；用 `RUST_LOG=debug` 看是否进入 body 改写日志 `M3U8 rewritten` |
| 高并发崩溃 | 检查 `ulimit -n`（应 ≥ 65535）与 `/proc/sys/fs/file-max`；必要时在 procd 配置里降低 WORKERS |

---

## 📄 许可证

MIT License

## 🙏 致谢

- [Pingora](https://github.com/cloudflare/pingora) - Cloudflare 高性能代理框架
- [urlencoding](https://github.com/nickshanks/urlencoding) / [url](https://github.com/servo/rust-url) - URL 编解码
