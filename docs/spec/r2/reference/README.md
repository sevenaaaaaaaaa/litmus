# litmus-r2 · Phase 0 产物

这是「鹿蕊 · 灵魂卡与全面重规划」Phase 0 的可跑产物。文档在 `Notes/产品/鹿蕊执行/`。

## 一条命令

```bash
bash tests/run.sh
```

会跑：卡状态机 8 条测试 → 原型打包 + DOM 冒烟 → 对比度审计 → 指标×埋点对账（6 条，含 4 条反例）
→ 分享队列协议 6 条测试 → 两个 Swift 文件 parse → Mac 快捕编译 → 200 篇合成库性能夹具。

## 目录

| 目录 | 是什么 | 怎么用 |
|---|---|---|
| `p0-01-events/` | 16 个埋点事件的字段与 PII 档位 | 接埋点时照它实现 |
| `p0-02-metrics/` | 10 个指标的公式 + 对账校验器 | `node p0-02-metrics/validate.mjs` |
| `p0-03-perf/` | 合成库生成器 + 测量脚本 | `node p0-03-perf/gen-vault.mjs --out /tmp/v --notes 2000` |
| `p0-05-card/` | 卡解剖只读原型（HTML） | `node p0-05-card/build.mjs` 后双击 `index.html` |
| `p0-06-mac-capture/` | Mac 快捕 NSPanel 原型 | `swiftc -O -o /tmp/quickcapture main.swift -framework AppKit -framework Carbon` |
| `p0-07-ios-share/` | 分享队列协议 + Swift 骨架 | `node --test p0-07-ios-share/queue.test.mjs` |
| `p0-08-soul-interview/` | 灵魂卡 6 问台本 | `node p0-08-soul-interview/run-interview.mjs` |
| `p1-01-soul/` | 灵魂卡解析/校验/diff/锚点重挂 | `node p1-01-soul/soul-cli.mjs validate p1-01-soul/soul-example.md` |
| `p1-11-citations/` | 三色引用校验 + 预览页 | `node p1-11-citations/demo.mjs` |
| `p1-03-actions/` | 五动作规格 + 校验器 + 速查表 | `node p1-03-actions/action-cli.mjs validate` |
| `p1-04-advance/` | 推进闭环 + 端点核实 | `node p1-04-advance/advance-cli.mjs` |
| `p1-02-soul-entrances/` | 灵魂卡三入口（访谈/提炼/粘贴） | `node p1-02-soul-entrances/demo.mjs` |
| `p1-05-backend/` | 可替换后端（文件真跑，端点留接口） | `node p1-05-backend/demo.mjs` |

## 诚实边界

- 原型不写真数据进真库；快捕只写 `Notes/Inbox/`（用 `LITMUS_VAULT` 指到临时目录跑）。
- iOS 两个 Swift 文件只做了 `-parse`（语法级），没有 Xcode 工程、没有 entitlements、没有真机。
- 性能只测了文件系统；App 冷启动/搜索/CPU/内存需要 App 在运行，命令见 `Notes/产品/鹿蕊执行/P0-03-性能基线.md`。