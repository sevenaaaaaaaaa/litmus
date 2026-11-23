#!/usr/bin/env bash
# 一条命令跑完 P0 的全部可验证物。失败即退非零。
set -u
cd "$(dirname "$0")/.."
fail=0
say() { printf '\n\033[1m== %s ==\033[0m\n' "$1"; }
ok()  { printf '  \033[32mPASS\033[0m %s\n' "$1"; }
bad() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=1; }

say "P0-05 卡状态机"
node --test p0-05-card/card-state.test.mjs >/tmp/t1.log 2>&1 && ok "8 条状态机测试" || { bad "状态机测试"; tail -20 /tmp/t1.log; }

say "P0-05 原型打包 + DOM 冒烟"
node p0-05-card/build.mjs >/tmp/t2.log 2>&1 && node --check p0-05-card/dist/prototype.js && ok "bundle 可解析" || { bad "bundle"; cat /tmp/t2.log; }
node --test p0-05-card/smoke.test.mjs >/tmp/t3.log 2>&1 && ok "DOM 冒烟（3 张卡渲染）" || { bad "DOM 冒烟"; tail -20 /tmp/t3.log; }

say "P0-05 显色状态对比度"
node --input-type=module -e "
import {audit} from './p0-05-card/contrast.mjs';
let bad=0;
for (const s of ['light','dark']) for (const r of audit(s)) {
  if (!r.pass) { bad++; console.log('  ✗', s, r.state, r.kind, r.onPaper+'/'+r.onSheet, '<', r.need); }
}
console.log('  对比度检查: ' + (bad? bad+' 项未达标' : '全部达标（边 ≥3:1，文字 ≥4.5:1）'));
process.exit(bad?1:0);
" && ok "对比度" || bad "对比度"

say "P0-02 指标 × 埋点对账"
node p0-02-metrics/validate.mjs >/tmp/t4.log 2>&1 && ok "$(grep -c '^WARN' /tmp/t4.log) 条提醒、0 条错误" || { bad "对账"; cat /tmp/t4.log; }
node --test p0-02-metrics/validate.test.mjs >/tmp/t5.log 2>&1 && ok "8 条对账测试（含 4 条反例）" || { bad "对账测试"; tail -20 /tmp/t5.log; }

say "P0-07 分享队列协议"
node --test p0-07-ios-share/queue.test.mjs >/tmp/t6.log 2>&1 && ok "6 条队列测试（幂等/原子/坏文件不吞）" || { bad "队列测试"; tail -20 /tmp/t6.log; }
for f in p0-07-ios-share/*.swift; do
  xcrun swiftc -parse "$f" 2>/tmp/t7.log && ok "parse $(basename "$f")" || { bad "parse $f"; head -10 /tmp/t7.log; }
done

say "P0-06 Mac 快捕编译"
swiftc -O -o /tmp/quickcapture p0-06-mac-capture/main.swift -framework AppKit -framework Carbon 2>/tmp/t8.log \
  && ok "swiftc 编译通过（$(stat -f%z /tmp/quickcapture) bytes）" || { bad "swiftc"; head -20 /tmp/t8.log; }

say "P0-03 性能夹具（200 篇 / 2000 块，快速档）"
rm -rf /tmp/litmus-perf-smoke
node p0-03-perf/gen-vault.mjs --out /tmp/litmus-perf-smoke --notes 200 --blocks 10 >/tmp/t9.log 2>&1 \
  && node p0-03-perf/measure.mjs --vault /tmp/litmus-perf-smoke --samples 100 >>/tmp/t9.log 2>&1 \
  && ok "夹具生成 + 测量可跑（$(grep -o '"files": [0-9]*' /tmp/t9.log | tail -1)）" || { bad "性能夹具"; tail -20 /tmp/t9.log; }

say "P1-02 灵魂卡三入口"
node p1-02-soul-entrances/demo.mjs >/tmp/t22.log 2>&1 && ok "三入口候选逐字可核；确认后落盘解 0 错；临时卡带出处" || { bad "三入口 demo"; tail -20 /tmp/t22.log; }
node --test p1-02-soul-entrances/entrances.test.mjs >/tmp/t23.log 2>&1 && ok "12 条（逐字可核/改写/删除/共用格式/临时卡/原子写）" || { bad "三入口测试"; tail -20 /tmp/t23.log; }

say "P1-05 可替换后端"
node p1-05-backend/demo.mjs >/tmp/t24.log 2>&1 && ok "文件后端真跑通；端点后端显式失败并列出待补参数" || { bad "后端 demo"; tail -20 /tmp/t24.log; }
node --test p1-05-backend/backends.test.mjs >/tmp/t25.log 2>&1 && ok "9 条（契约/能力诚实/不静默降级）" || { bad "后端测试"; tail -20 /tmp/t25.log; }

say "P1-04 推进闭环 + 端点核实"
node p1-04-advance/advance-cli.mjs >/tmp/t18.log 2>&1 && ok "端到端：取消零写入 / 无来源拦截 / 落盘连回 / 幂等 / 逐字节撤销" || { bad "闭环 demo"; tail -20 /tmp/t18.log; }
node --test p1-04-advance/advance.test.mjs >/tmp/t19.log 2>&1 && ok "14 条闭环测试" || { bad "闭环测试"; tail -20 /tmp/t19.log; }
node p1-04-advance/verify-endpoints.mjs >/tmp/t20.log 2>&1 && ok "端点核实：$(grep -c '^OK' /tmp/t20.log) 条命中 / $(grep -c '^FAIL' /tmp/t20.log) 条不符" || { bad "端点核实"; cat /tmp/t20.log; }
node --test p1-04-advance/endpoints.test.mjs >/tmp/t21.log 2>&1 && ok "6 条端点测试" || { bad "端点测试"; tail -20 /tmp/t21.log; }

say "P1-03 五动作规格"
node p1-03-actions/action-cli.mjs validate >/tmp/t15.log 2>&1 && ok "$(cat /tmp/t15.log)" || { bad "动作规格"; cat /tmp/t15.log; }
node p1-03-actions/action-cli.mjs sheet --out dist/reference.md >/tmp/t16.log 2>&1 && ok "速查表已生成（dist/reference.md）" || { bad "速查表"; cat /tmp/t16.log; }
node --test p1-03-actions/actions.test.mjs >/tmp/t17.log 2>&1 && ok "17 条（键位冲突/保留键/落点/撤销/保稿/等价/跨模块埋点）" || { bad "动作测试"; tail -20 /tmp/t17.log; }

say "P1-11 证据与三色引用"
node p1-11-citations/demo.mjs >/tmp/t13.log 2>&1 && ok "端到端：6 条结论校验完，预览页已生成" || { bad "引用 demo"; tail -20 /tmp/t13.log; }
node --test p1-11-citations/citations.test.mjs >/tmp/t14.log 2>&1 && ok "15 条（三色/失败分型/固定文案/转义/对比度/点回原文）" || { bad "引用测试"; tail -20 /tmp/t14.log; }

say "P1-01 灵魂卡模型 + 块锚点"
node --test p1-01-soul/soul.test.mjs >/tmp/t10.log 2>&1 && ok "17 条解析/校验/重挂测试" || { bad "灵魂卡测试"; tail -20 /tmp/t10.log; }
node p1-01-soul/soul-cli.mjs validate p1-01-soul/soul-example.md >/tmp/t11.log 2>&1 && ok "示例卡 VALID（$(grep -o '[0-9]* 条' /tmp/t11.log | head -1)）" || { bad "示例卡"; cat /tmp/t11.log; }
node p1-01-soul/soul-cli.mjs validate p1-01-soul/soul-template.md >/tmp/t12.log 2>&1 && ok "临时卡 VALID（confirmed:false）" || { bad "临时卡"; cat /tmp/t12.log; }

echo
if [ "$fail" -eq 0 ]; then printf '\033[32mALL PASS\033[0m\n'; else printf '\033[31mSOME FAILED\033[0m\n'; fi
exit "$fail"
