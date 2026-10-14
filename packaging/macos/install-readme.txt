@APP_NAME@ 安装说明（macOS）
==============================

本安装包未经 Apple 公证，首次打开时 macOS 可能提示
「已损坏，无法打开，您应该将它移到废纸篓」。应用本身没有问题，按下面步骤即可：

1. 把 @APP_NAME@.app 拖到右侧的「Applications（应用程序）」文件夹。

2. 双击「打不开？双击我.command」。
   它会移除 macOS 给下载文件加的「隔离」标记，然后自动打开 @APP_NAME@。
   全程不联网、不改动其他系统设置。

3. 如果 macOS 连这个 .command 也拦住（提示无法验证开发者）：
   - 在它上面 右键（或按住 Control 点按）→ 打开 → 再点「打开」；或
   - 打开「系统设置 → 隐私与安全性」，在页面下方找到被拦截的项目，点「仍要打开」。

4. 也可以打开「终端」，执行这一行命令（效果相同）：

   xattr -dr com.apple.quarantine "/Applications/@APP_NAME@.app" && open "/Applications/@APP_NAME@.app"

只需要做一次。之后 @APP_NAME@ 会在应用内自动检查并安装更新。
