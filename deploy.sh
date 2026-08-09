#!/bin/bash

set -e

echo "=== 1. 编译Pingora IPTV代理 ==="
cd /home/tetora/Desktop/fnos/pingora-iptv
cargo build --release

echo ""
echo "=== 2. 上传到192.168.1.166 ==="
sshpass -p '123456tyui' scp target/release/iptv-proxy noreply@192.168.1.166:/tmp/

echo ""
echo "=== 3. 在远程服务器上配置 ==="
sshpass -p '123456tyui' ssh noreply@192.168.1.166 << 'REMOTE'
echo 123456tyui | sudo -S tee /etc/systemd/system/iptv-proxy.service > /dev/null << 'SERVICE'
[Unit]
Description=IPTV Pingora Proxy
After=network.target

[Service]
Type=simple
User=root
ExecStart=/usr/local/bin/iptv-proxy
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
SERVICE

echo 123456tyui | sudo -S mv /tmp/iptv-proxy /usr/local/bin/
echo 123456tyui | sudo -S chmod +x /usr/local/bin/iptv-proxy
echo 123456tyui | sudo -S systemctl daemon-reload
echo 123456tyui | sudo -S systemctl enable iptv-proxy
echo 123456tyui | sudo -S systemctl restart iptv-proxy

sleep 2
echo ""
echo "=== 检查状态 ==="
echo 123456tyui | sudo -S systemctl status iptv-proxy | grep Active
REMOTE

echo ""
echo "=== 4. 测试 ==="
sleep 2
curl -s "http://192.168.1.166:8080/iptv/http://116.199.7.27:8006/00000000/1d77ac8593854801b7503a85270ee7b9/index.m3u8" | head -10

echo ""
echo "✅ 部署完成！"
echo ""
echo "播放地址："
echo "  http://192.168.1.166:8080/iptv/http://116.199.7.27:8006/xxx/index.m3u8"
