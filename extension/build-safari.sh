#!/usr/bin/env bash
# Safari Web Extension 转换（需要 Xcode）
# 参考：https://developer.apple.com/documentation/safariservices/safari_web_extensions
echo "1. 打开 Xcode → File → New → Project → macOS → Safari Extension App"
echo "2. 把 extension/ 目录的内容复制到 extension resources 里"
echo "3. 或者用命令行："
echo "   xcrun safari-web-extension-converter extension/ --app-name Litmus --bundle-identifier com.nownexts.litmus"
echo "4. 在 Safari → 设置 → 扩展 里启用"
echo ""
echo "或直接运行："
echo "  xcrun safari-web-extension-converter $(pwd)/extension --app-name 'Litmus' --bundle-identifier 'com.nownexts.litmus' --no-open"
