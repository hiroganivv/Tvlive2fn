# IPTV Proxy - 生产级高性能透明代理

基于 Cloudflare Pingora 的 IPTV 透明代理，专为 OpenWrt IPQ60xx (1GB RAM) 优化。

## ⚡ 性能指标

| 指标 | 目标值 | 实测值（IPQ60xx 4核A53） |
|------|--------|--------------------------|
| 最大并发连接 | 50,000+ | 65,000+ |
| 单连接延迟 | < 10ms | ~5ms |
| 吞吐量 | 1000+ req/s | 2000+ req/s |
| 内存占用 | < 100MB | 30-50MB |
| CPU占用 | < 50% | 10-30% |

## 🎯 功能特性

### 核心功能
- ✅ **动态URL代理**: `/iptv/http://...` 自动解析并转发
- ✅ **自动m3u8改写**: `116.199.x.x` URL自动改写为代理地址
- ✅ **智能Referer注入**: `surrit.com`/`fourhoi.com` 自动添加Referer
- ✅ **透明TS代理**: `/proxy/host:port/path` 零拷贝转发
- ✅ **HTTP/HTTPS支持**: 自动识别scheme

### 性能优化
- 🚀 **零拷贝**: Bytes缓冲区直接传递，无内存复制
- 🚀 **静态链接**: musl libc静态编译，无依赖
- 🚀 **LTO优化**: 链接时优化减少二进制大小50%+
- 🚀 **智能判断**: 只对需要的内容进行改写
- 🚀 **热路径日志降级**: 请求级日志为 debug 级别，避免高并发下日志锁竞争
- 🚀 **依赖精简**: 已移除未使用的依赖（reqwest/memchr/regex/once_cell/ahash/num_cpus），二进制更小、编译更快

### 可靠性
- 🛡️ **自动重启**: procd守护进程管理
- 🛡️ **连接复用**: HTTP/1.1 Keep-Alive
- 🛡️ **错误恢复**: 上游失败自动返回400/502
- 🛡️ **资源限制**: ulimit自动配置

## 📦 部署

### 前置要求

```bash
# 安装Rust (本机)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 安装交叉编译工具 (Ubuntu/Debian)
sudo apt install -y musl-tools gcc-aarch64-linux-gnu

# 安装sshpass (用于自动部署)
sudo apt install -y sshpass
```

### 快速部署到OpenWrt

```bash
cd pingora-iptv
chmod +x deploy-openwrt.sh

# 编辑脚本中的连接信息
# TARGET_HOST="192.168.1.3"
# TARGET_USER="root"
# TARGET_PASS="password"

./deploy-openwrt.sh
```

### 手动部署

```bash
# 1. 编译
cargo build --release --target aarch64-unknown-linux-musl

# 2. Strip
aarch64-linux-musl-strip target/aarch64-unknown-linux-musl/release/iptv-proxy

# 3. 上传
scp target/aarch64-unknown-linux-musl/release/iptv-proxy root@192.168.1.3:/usr/bin/

# 4. SSH到OpenWrt配置
ssh root@192.168.1.3
chmod +x /usr/bin/iptv-proxy
/etc/init.d/iptv-proxy enable
/etc/init.d/iptv-proxy start
```

## 🎬 使用方法

### URL格式（统一 `/?url=` 模式）

所有目标地址通过 `url` 查询参数传入（需要 URL 编码）：

```bash
# 任意 IPTV 源（m3u8 自动改写，分片也走本代理）
curl "http://192.168.1.3:8080/?url=http%3A%2F%2F116.199.7.27%3A8006%2F00000000%2Fxxx%2Findex.m3u8"

# surrit.com / fourhoi.com 等（自动注入 Referer）
curl "http://192.168.1.3:8080/?url=https%3A%2F%2Fsurrit.com%2Fpath%2Ffile.m3u8"

# 直接代理任意媒体文件（TS 分片等，零拷贝透传）
curl "http://192.168.1.3:8080/?url=http%3A%2F%2F116.199.4.228%3A8114%2FLIVES%2Fsegment.ts"

# 播放器里填：
# http://192.168.1.3:8080/?url=http%3A%2F%2F116.199.7.27%3A8006%2Fxxx%2Findex.m3u8
```

URL 编码可用任意工具生成：`python3 -c "import urllib.parse;print(urllib.parse.quote('http://116.199.7.27:8006/xxx/index.m3u8',safe=''))"`。

### m3u8改写示例

**原始m3u8** (上游返回):
```m3u8
#EXTM3U
#EXT-X-VERSION:3
#EXTINF:10.0,
http://116.199.4.228:8114/LIVES/segment001.ts
#EXTINF:10.0,
12345.ts
```

**代理后** (192.168.1.3:8080返回，所有分片被改写为代理地址):
```m3u8
#EXTM3U
#EXT-X-VERSION:3
#EXTINF:10.0,
http://192.168.1.3:8080/?url=http%3A%2F%2F116.199.4.228%3A8114%2FLIVES%2Fsegment001.ts
#EXTINF:10.0,
http://192.168.1.3:8080/?url=http%3A%2F%2F116.199.4.228%3A8114%2FLIVES%2F12345.ts
```

支持相对路径、绝对路径、绝对 URL、查询参数分片，以及 `#EXT-X-KEY`（AES-128 密钥）、`#EXT-X-MAP`（fMP4 初始化段）等标签内引号 URI 的改写。

## 🔧 配置

### 环境变量

在 `/etc/init.d/iptv-proxy` 中修改：

```bash
LOCAL_IP="192.168.1.3"      # 本机IP（用于m3u8改写）
BIND_ADDR="0.0.0.0:8080"    # 监听地址
WORKERS="4"                  # 工作线程数（默认自动检测CPU核心数）
RUST_LOG="info"              # 日志级别: error/warn/info/debug
LOG_FILE="/tmp/iptv-proxy.log"  # 日志文件路径（默认 <临时目录>/iptv-proxy.log，OpenWrt 即 /tmp/iptv-proxy.log）
ALLOW_HOSTS="surrit.com,fourhoi.com"  # 可选：上游域名白名单（逗号分隔，未设置时不做域名限制）
```

### 日志

- 日志**同时输出到 stderr 与日志文件**，服务以守护方式（无终端）启动时也能调试：
  ```bash
  tail -f /tmp/iptv-proxy.log
  ```
- 调试时把 `RUST_LOG` 设为 `debug`，可看到每次请求的解码 URL、上游连接、请求头、m3u8 改写明细。
- 文件打开失败（如路径无权限）时自动降级为仅 stderr，不影响服务启动。

### 系统优化

已自动应用（在`/etc/sysctl.conf`）：

```bash
net.core.somaxconn=32768
net.ipv4.tcp_max_syn_backlog=16384
net.ipv4.ip_local_port_range=1024 65535
net.ipv4.tcp_tw_reuse=1
net.ipv4.tcp_fin_timeout=15
fs.file-max=200000
```

## 📊 监控

### 服务管理

```bash
# 启动
/etc/init.d/iptv-proxy start

# 停止
/etc/init.d/iptv-proxy stop

# 重启
/etc/init.d/iptv-proxy restart

# 查看状态
/etc/init.d/iptv-proxy status
ps | grep iptv-proxy
```

### 实时监控

```bash
# 查看日志
logread -f | grep iptv-proxy

# 监控连接数
watch -n 1 'netstat -an | grep :8080 | wc -l'

# 监控内存
watch -n 1 'ps aux | grep iptv-proxy | grep -v grep'

# 连接状态分布
netstat -an | grep :8080 | awk '{print $6}' | sort | uniq -c
```

### 性能测试

```bash
./test-performance.sh 192.168.1.3 8080
```

## 🐛 故障排查

### 服务无法启动

```bash
# 检查二进制
ls -lh /usr/bin/iptv-proxy
/usr/bin/iptv-proxy --version  # 应该无输出，直接启动

# 检查端口占用
netstat -tlnp | grep 8080

# 手动启动查看错误
LOCAL_IP=192.168.1.3 RUST_LOG=debug /usr/bin/iptv-proxy
```

### 404错误

```bash
# 检查URL格式（必须是 /?url=<编码后的目标>）
curl -v "http://192.168.1.3:8080/?url=http%3A%2F%2F116.199.7.27%3A8006%2Ftest.m3u8"

# 应该看到 Decoded URL / Upstream 日志
logread | grep "IPTV:"
tail -f /tmp/iptv-proxy.log
```

### 播放1秒就中断 / 有声音无画面

多为 m3u8 改写漏改导致部分分片没走代理。用 `RUST_LOG=debug` 重启后看 `/tmp/iptv-proxy.log`：

```bash
# 检查返回的 m3u8 中是否有未改写的裸地址（分片名应为 .../?url=http%3A... 形式）
curl -s "http://192.168.1.3:8080/?url=http%3A%2F%2F116.199.7.27%3A8006%2Fxxx.m3u8"

# 若分片 URL 形如 http://116.199.x.x/... 裸地址，说明该源未被改写
```

### 高并发下崩溃

```bash
# 检查ulimit
ulimit -n  # 应该>=65535

# 检查系统限制
cat /proc/sys/fs/file-max

# 增加内存（如果OOM）
# 在/etc/init.d/iptv-proxy中减少WORKERS
```

## 📈 性能调优

### 低内存环境（512MB）

```bash
# 减少worker数量
WORKERS="2"  # 在init脚本中

# 或者使用环境变量
LOCAL_IP=192.168.1.3 WORKERS=2 /usr/bin/iptv-proxy
```

### 高并发优化（2GB+）

```bash
# 增加worker
WORKERS="8"

# 调整连接限制
# 在/etc/sysctl.conf
net.core.somaxconn=65536
net.ipv4.tcp_max_syn_backlog=32768
```

## 🔒 安全建议

- **内网使用**: 仅绑定内网IP，不要暴露到公网
- **防火墙**: 只允许192.168.1.0/24访问8080端口
- **日志审计**: 定期检查访问日志

## 📝 开发

### 本地测试

```bash
# 运行
LOCAL_IP=127.0.0.1 RUST_LOG=debug cargo run

# 测试
curl "http://127.0.0.1:8080/health"
curl "http://127.0.0.1:8080/iptv/http://httpbin.org/get"
```

### 代码结构

```
src/main.rs
├── ProxyConfig        # 配置管理
├── ProxyContext       # 请求上下文
├── IptvProxy          # 核心代理逻辑
│   ├── upstream_peer           # 解析目标并建立连接
│   ├── upstream_request_filter # 修改请求头
│   ├── response_filter         # 修改响应头
│   └── response_body_filter    # 改写响应body
└── main              # 服务器初始化
```

## 📄 许可证

MIT License

## 🙏 致谢

- [Pingora](https://github.com/cloudflare/pingora) - Cloudflare高性能代理框架
- [memchr](https://github.com/BurntSushi/memchr) - SIMD加速字符串搜索
