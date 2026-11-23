---
title: P0-07 · iOS 分享扩展原型
date: 2026-11-23
status: done
tags: [执行, iOS, 同步]
---

# P0-07 · iOS 分享扩展原型

## 目标

> iOS 分享扩展原型（App Group 文件队列）。离线可捕捉；失败不丢。

## 做了什么

产物：[QueueFile.swift](reference/p0-07-ios-share/QueueFile.swift) · [ShareViewController.swift](reference/p0-07-ios-share/ShareViewController.swift) · [queue.mjs](reference/p0-07-ios-share/queue.mjs)

**队列协议**（Mac 与 iOS 共用一份定义）：

```
<App Group>/litmus/queue/2026-11-23T13-04-00Z-<id>.json
<App Group>/litmus/queue/.tmp/<uuid>.json      ← 先写这里，再 rename
```

| 纪律 | 实现 | 验证 |
|---|---|---|
| 原子写 | 写 `.tmp` → `rename`（同卷，原子） | 成功入队后 `.tmp` 为空（有测试） |
| 幂等 | id = sha256(kind + text + 分钟桶) | 同分钟同内容第二次返回 false（有测试） |
| 坏文件不吞 | 解析失败跳过、**不删除** | 写一个坏 JSON，drain 仍返回好的一条（有测试） |
| 顺序 | 文件名以 ISO 时间开头，字典序即时间序 | 两条内容读回顺序正确（有测试） |
| 失败留稿 | Swift 侧 catch 后不清空输入，状态行给可读原因 | 代码路径已写；未在真机跑 |

验证：`node --test p0-07-ios-share/queue.test.mjs` → 6/6；两个 Swift 文件 `swiftc -parse` 通过（语法级，不是编译级——缺 iOS SDK 的 UIKit 链接）。

## 还没做（诚实清单）

- **Entitlements 未建**：`group.com.understory.litmus` 需要在 Xcode 工程里声明，原型只有代码。
- **SHA256 是占位实现**（FNV 变体）：正式版必须用 CryptoKit。
- **附件没进队列**：图片/PDF 只留了 `attachments` 字段，没有真正拷贝与校验和。
- **没有回执**：Mac 处理完不会告诉 iPhone"已收到"。

## 新思路

1. **分钟桶会把"同一分钟改一个字"判成重复。** 用户分享后立刻发现错字、改一下再分享，第二次会被静默丢掉——最糟的失败是无声的。幂等键应该只去重"完全相同的内容"，改过的算新条目，重复提示放到 UI 层。
2. **单向队列缺回执，用户永远不知道有没有到 Mac。** 需要一个极小的 `ack/<id>` 文件（Mac 处理完写回），iPhone 下次打开时读；没有 ack 的项显示"等待同步"而不是"已保存"。
3. **附件不能塞进 JSON。** 队列项应该带 `attachments: [{path, sha256, bytes}]`，先在 App Group 里落文件再入队；否则用户分享一张 20MB 的图，队列项仍然"成功"但内容是空的。
4. **分享扩展会被系统杀掉（约 30 秒）。** 所以"保存成功"只能依赖**本地写盘**，同步必须后置；任何在上传里等结果的实现都会随机丢内容。
5. **App Group 目录会被 iCloud 备份。** 分享的内容可能包含隐私材料，队列目录应标 `isExcludedFromBackup`——"本地优先"要在文件属性上兑现，不只在文案里。

## 对后续的影响

- P1-04 推进闭环的"离线排队"可以复用这套队列协议（同一份 id 规则）。
- P2-06 发布回流若要走 iPhone，也要 ack 机制，不能假设单向到达。
