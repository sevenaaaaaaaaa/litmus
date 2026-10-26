# Litmus Clipper — 浏览器扩展

一键把网页（整页/选区/截图）采集到你的 Litmus 知识库。

## 安装

### Chrome / Edge / Brave / Arc
1. 下载或克隆本仓库
2. 打开 `chrome://extensions`（Edge: `edge://extensions`）
3. 右上角开启 **开发者模式**
4. 点击 **加载已解压的扩展程序** → 选择 `extension/` 目录
5. 点击工具栏 3C 图标 → 填入 Litmus 地址（如 `https://nownexts.com/litmus`）和 Token → 保存

### Firefox
1. 打开 `about:debugging#/runtime/this-firefox`
2. 点击 **临时载入附加组件** → 选择 `extension-firefox/manifest.json`
3. 配置同上（弹出窗口自动打开）

### Safari
1. 确保安装了 Xcode
2. 运行：
   ```bash
   xcrun safari-web-extension-converter extension/ --app-name Litmus --bundle-identifier com.nownexts.litmus --no-open
   ```
3. Xcode 会打开项目 → 点 Run → Safari 自动打开
4. Safari → 设置 → 扩展 → 勾选 Litmus Clipper

## 使用

1. 打开任意网页
2. **右键 → 「采集到 Litmus」**：
   - 无选区 → 采集整页（URL + 标题 + 正文）
   - 有选区 → 只采集选中的文字
3. 右下角通知确认采集结果
4. 打开 Litmus → 检索刚采集的内容

## 配置

弹出窗口两个字段：

| 字段 | 说明 | 示例 |
|---|---|---|
| Litmus 地址 | 你的 daemon 地址 | `https://nownexts.com/litmus` 或 `http://127.0.0.1:7700` |
| Token | API 令牌（库的 machine.toml 里的 token 字段） | `01m2wc48hef2b2t07hwk96qmej...` |

配置存储在 `chrome.storage.local`（Firefox 同理），**不上传任何地方**。

## 隐私

- 扩展只在你右键时读取当前页面，**不后台监听浏览记录**
- 数据直接从浏览器发到你自己的 Litmus 实例，**不经过第三方**
- 密钥只存在本地 storage

## 文件

```
extension/
├── manifest.json     Chrome/Edge MV3
├── bg.js             右键菜单 + 采集逻辑
├── popup.html/js     配置面板
└── icon48.png        图标
extension-firefox/    Firefox 适配版（browser.* API + .xpi 包）
build-safari.sh       Safari 转换脚本
```
