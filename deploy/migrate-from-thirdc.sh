#!/usr/bin/env bash
# 服务器一次性迁移：thirdc → litmus（目录、systemd 服务、Apache 反代）。幂等，可重跑。
# 用法（在服务器上，root）：bash /www/wwwroot/litmus/src/deploy/migrate-from-thirdc.sh
#   或从本机：ssh -p 28766 root@172.96.253.73 'bash -s' < deploy/migrate-from-thirdc.sh
# 之后照常 ./deploy/deploy.sh 部署新版本；新版本第一次打开 kb 时会把 thirdc.toml / .thirdc/ 自动改名。
set -euo pipefail
OLD=/www/wwwroot/thirdc; NEW=/www/wwwroot/litmus
OLD_OPS=/www/ops/thirdc; NEW_OPS=/www/ops/litmus
VHOST=/www/server/panel/vhost/apache/extension/nownexts.com
say(){ printf '\n▶ %s\n' "$*"; }

say "1/5 停掉旧服务 thirdc"
systemctl stop thirdc 2>/dev/null || true
systemctl disable thirdc 2>/dev/null || true

say "2/5 目录改名（旧路径留一个指向新目录的软链接，别处写死的旧路径照样能用）"
if [ -d "$OLD" ] && [ ! -L "$OLD" ] && [ ! -e "$NEW" ]; then mv "$OLD" "$NEW"; fi
[ -e "$OLD" ] || ln -s "$NEW" "$OLD"
if [ -d "$OLD_OPS" ] && [ ! -L "$OLD_OPS" ] && [ ! -e "$NEW_OPS" ]; then mv "$OLD_OPS" "$NEW_OPS"; fi
[ -e "$OLD_OPS" ] || { [ -d "$NEW_OPS" ] && ln -s "$NEW_OPS" "$OLD_OPS"; } || true
# 新二进制还没部署时，先用旧的顶上（旧二进制照旧读 thirdc.toml，库不受影响）
if [ ! -x "$NEW/bin/litmus" ] && [ -x "$NEW/bin/thirdc" ]; then cp "$NEW/bin/thirdc" "$NEW/bin/litmus"; fi
[ -f "$NEW/update.sh" ] && sed -i 's#/www/wwwroot/thirdc#/www/wwwroot/litmus#g; s#target/release/thirdc#target/release/litmus#g; s#bin/thirdc#bin/litmus#g; s#systemctl \(stop\|start\|restart\) thirdc#systemctl \1 litmus#g; s#-p thirdc\b#-p litmus#g' "$NEW/update.sh"

say "3/5 安装 systemd 服务 litmus"
cat > /etc/systemd/system/litmus.service <<UNIT
[Unit]
Description=Litmus knowledge kernel
After=network.target
[Service]
WorkingDirectory=$NEW
ExecStart=$NEW/bin/litmus serve $NEW/kb --addr 127.0.0.1:7700
Restart=always
$(grep -h '^Environment=' /etc/systemd/system/thirdc.service 2>/dev/null | sed 's/THIRDC_/LITMUS_/g' || true)
[Install]
WantedBy=multi-user.target
UNIT
rm -f /etc/systemd/system/thirdc.service
systemctl daemon-reload
systemctl enable --now litmus
sleep 2
systemctl is-active litmus

say "4/5 Apache：/litmus 为正式地址，/thirdc 页面跳转、接口直通"
mkdir -p "$VHOST"
cat > "$VHOST/litmus.conf" <<'CONF'
Alias /litmus /www/wwwroot/litmus/web
ProxyPass /litmus/ws ws://127.0.0.1:7700/ws
ProxyPass /litmus/ http://127.0.0.1:7700/
ProxyPassReverse /litmus/ http://127.0.0.1:7700/
ProxyPassMatch ^/thirdc/?$ !
RedirectMatch 301 ^/thirdc/?$ /litmus/
ProxyPass /thirdc/ws ws://127.0.0.1:7700/ws
ProxyPass /thirdc/ http://127.0.0.1:7700/
ProxyPassReverse /thirdc/ http://127.0.0.1:7700/
CONF
rm -f "$VHOST/thirdc.conf"
if command -v apachectl >/dev/null; then A=apachectl; else A=/www/server/apache/bin/apachectl; fi
$A -t && $A graceful

say "5/5 验证"
curl -s -o /dev/null -w "   本机 :7700/        → %{http_code}\n" http://127.0.0.1:7700/ || true
curl -s -o /dev/null -w "   https …/litmus/    → %{http_code}\n" https://nownexts.com/litmus/ || true
curl -s -o /dev/null -w "   https …/thirdc/    → %{http_code}（应为 301）\n" https://nownexts.com/thirdc/ || true
echo "✓ 服务器迁移完成。接着在本机跑 ./deploy/deploy.sh 部署新版本。"
