#!/usr/bin/env bash
# Litmus 原生部署（不用 Docker）：rsync 源码 → 服务器上 cargo build → systemd 托管
set -euo pipefail
SSH_KEY="$(dirname "$0")/.ssh_key"
SSH_PORT="28766"; USER_="root"; HOST="172.96.253.73"
REMOTE_DIR="/www/wwwroot/litmus"
SSH_CMD="ssh -i ${SSH_KEY} -p ${SSH_PORT} -o StrictHostKeyChecking=accept-new ${USER_}@${HOST}"

echo "① 同步源码..."
tar --exclude=.git --exclude=target --exclude=.litmus/local --exclude='*.db' -cf - . | \
  ${SSH_CMD} "mkdir -p ${REMOTE_DIR}/src && tar -xf - -C ${REMOTE_DIR}/src/"

echo "② 服务器端安装 Rust（如果没有）..."
${SSH_CMD} "command -v cargo || (curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && source ~/.cargo/env)"

echo "③ 构建（首次约 10-15 分钟）..."
${SSH_CMD} "source ~/.cargo/env 2>/dev/null; export PATH=\$HOME/.cargo/bin:\$PATH; cd ${REMOTE_DIR}/src && cargo build --release -p litmus 2>&1 | tail -3"

echo "④ 部署二进制 + 初始化库 + systemd..."
${SSH_CMD} "mkdir -p ${REMOTE_DIR}/bin ${REMOTE_DIR}/kb
  cp ${REMOTE_DIR}/src/target/release/litmus ${REMOTE_DIR}/bin/
  cd ${REMOTE_DIR} && ./bin/litmus init kb Litmus-KB 2>/dev/null || true"

echo "⑤ systemd..."
${SSH_CMD} "cat > /etc/systemd/system/litmus.service <<'UNIT'
[Unit]
Description=Litmus knowledge kernel
After=network.target
[Service]
WorkingDirectory=/www/wwwroot/litmus
ExecStart=/www/wwwroot/litmus/bin/litmus serve /www/wwwroot/litmus/kb --addr 127.0.0.1:7700
Restart=always
RestartSec=3
Environment=LITMUS_AI_API_KEY=
[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload && systemctl enable --now litmus"

echo "⑥ Apache 代理（已配）..."
echo "✅ 部署完成: https://nownexts.com/litmus/"
