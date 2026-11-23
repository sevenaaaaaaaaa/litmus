---
title: P0-03 · 性能基线测试法
date: 2026-11-23
status: done
tags: [执行, 性能]
---

# P0-03 · 性能基线测试法

## 目标

> 性能基线：1 万篇库、冷启动、搜索、快捕、内存、CPU。

## 做了什么

产物：[gen-vault.mjs](reference/p0-03-perf/gen-vault.mjs) · [measure.mjs](reference/p0-03-perf/measure.mjs)

- **合成库生成器**：可配篇数、每篇块数；中英混排 + 引用块 + 待办 + wikilink，避免只测"纯文本最好情况"。
- **测量脚本**：文件遍历、单文件读、内核三个端点；测不了的打印 `not_measured` 与具体命令，不编数字。

## 真实数字（本机实测，2026-11-23）

```
合成库：2000 篇 / 20000 块 / 5.7 MB，生成 854 ms
遍历 2000 个 md：9.6 ms
单文件读：p50 0.09 ms / p95 0.18 ms
内核 /health/status、/docs/tree、/search/hybrid：fetch failed（App 未运行）
```

**结论：瓶颈不在文件 IO。** 0.18ms 的 p95 说明 1 万篇的读取根本不是问题，问题会在索引、watcher 和渲染。性能预算写成一个"冷启动 1.2s"会把这三段混在一起，出问题也定位不到。

## 未测清单（命令已写好，需要 App 在跑）

```bash
# 冷启动
killall litmus-desktop; time open -a Litmus
# ⌥Space 到可输入（原型进程读自己的日志）
LITMUS_VAULT=~/Documents/Litmus /tmp/quickcapture &
cat ~/Documents/Litmus/.litmus/local/ops/quickcapture.log
# 搜索 p95（1 万篇合成库挂上去之后）
node p0-03-perf/measure.mjs --vault /tmp/litmus-perf-vault --samples 500
# CPU / 内存
ps -o %cpu,rss,command -p $(pgrep -f litmus-desktop | head -1)
```

## 新思路

1. **合成库太"干净"。** 没有附件、没有 20MB 的 PDF 提取文本、没有超长单行。需要给生成器加"附件权重"和"脏数据比例"，否则 1 万篇跑得飞快，真库一开就卡。
2. **性能要分三段报，不要一个总分。** `IO / 索引 / 首帧` 三段各自有预算，否则优化了渲染却以为是索引慢。
3. **1 万篇的瓶颈多半是 watcher。** fsevents 在大库上会回调风暴；应该单独压"一次 git checkout 改 500 个文件"的场景。
4. **测量必须留档对比。** 每次把 JSON 存进 `perf/baseline-<date>.json`，下次 diff 超过 10% 就报警；否则"上次好像更快"会变成口头传说。
5. **"空闲 CPU < 1%" 不能用一次 ps 判断。** 需要 10 分钟采样（1s 一次，取中位与 p95），否则抓到的是启动瞬间。

## 对后续的影响

- P1-05 参考实现要做幂等，最好也带一个"重复执行 100 次"的耗时抽屉，顺手成为写盘性能的基线。
