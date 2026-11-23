---
title: P0-06 · Mac 快捕原型
date: 2026-11-23
status: done
tags: [执行, Mac, 原生]
---

# P0-06 · Mac 快捕原型

## 目标

> Mac 快捕原型（NSPanel + 全局快捷键）；验收 `⌥Space 到可输入 < 200ms`。

## 做了什么

产物：[main.swift](reference/p0-06-mac-capture/main.swift)（约 160 行，单文件可编译）

```bash
swiftc -O -o /tmp/quickcapture main.swift -framework AppKit -framework Carbon   # 通过，93,488 bytes
LITMUS_VAULT=/tmp/qc-vault /tmp/quickcapture &                                   # 实测启动
cat /tmp/qc-vault/.litmus/local/ops/quickcapture.log                             # ready vault=/tmp/qc-vault
```

| 设计点 | 做法 | 为什么 |
|---|---|---|
| 冷/热路径分开 | 启动时把 panel、输入框、约束全部建好并隐藏；热键只做 `makeKeyAndOrderFront` + `makeFirstResponder` | 热键时要做的越少越好；建视图是最贵的 |
| 不抢前台 | `.nonactivatingPanel` + `canBecomeKey` + `.floating` + `canJoinAllSpaces` | 全屏 App 上也能出现；不把用户从当前工作里拽出来 |
| 延迟有测量点 | 热键回调记 `shownAt`，下一个 runloop 算 `shown_to_focus`，写日志不弹 UI | 200ms 是可以被证伪的数字，不是形容词 |
| 失败保稿 | 写盘失败不清空输入，状态行给出可读原因 | 丢一段想法比慢 200ms 严重得多 |
| 两条提交路径 | ↩ 直接落卡到 `Notes/Inbox/`；⌘↩ 标记 `triage: requested` | 分诊永远是异步的，不阻塞保存 |

## 还没做（诚实清单）

- **全局热键只在应用激活时稳**：`RegisterEventHotKey` 在别的 App 独占全屏时可能收不到，需要 CGEventTap 兜底 + 辅助功能授权。
- **中文输入法**：非激活面板首次呼出仍可能闪一下输入法候选框，需要预热输入上下文。
- **没有 Dock 图标 / 菜单栏项**：`.accessory` 模式下用户不知道它在跑。正式版需要菜单栏常驻（今日三张 / 同步状态）。
- **签名与公证**：现在只是个可执行文件，没签名、没公证、没有自动更新。

## 新思路

1. **200ms 的真实瓶颈不是视图，是"激活 + 输入法"。** 非激活面板省掉了前台切换，但中文输入上下文第一次仍会掉帧。应该在应用启动时就预热一次输入法（用一个隐藏输入框），而不是等用户第一次按 ⌥Space。
2. **热键要两条腿。** `RegisterEventHotKey` 简单但会被全屏 App 吃掉；`CGEventTap` 能兜底但要辅助功能权限。产品上应该：默认免权限方案，检测到失效时再引导授权——而不是第一次就要求权限。
3. **"失败保稿"在原型里是假的。** 不清空输入只能抗"写盘失败"，抗不了崩溃。真正的保稿要每 800ms 把草稿写进 `.litmus/local/drafts/`，启动时问一句"要恢复吗"。
4. **分诊必须彻底异步。** ⌘↩ 会触发一次模型调用；如果 200ms 内没返回，用户已经关窗走了。分诊结果应该写回卡背并给一条通知，而不是等在这次交互里。
5. **快捕要记住"上次落在哪"。** 每次都进 Inbox，用户还得二次搬运——这违背"3 秒落下"的精神。默认空间应继承上一张卡，Inbox 只是兜底。

## 对后续的影响

- P1-03 五动作里「落下」的失败语义以此为准（保稿 > 快速）。
- P1-05 的幂等键可以复用 `quickcapture` 的时间戳命名规则，避免同名覆盖。
