# Memory Ability Taxonomy For LongMemEval And BEAM

这份说明定义了一个统一的 6-bucket taxonomy，用来把 LongMemEval 和 BEAM 的原始能力标签归一化。

注意：

- **主汇报路径** 仍然应该是 LongMemEval 和 BEAM 各自按官方能力分类分别汇报
- 这个 taxonomy 更适合作为内部分析、横向比较、回归观察的辅助视角

原则：

- 原始 benchmark 标签保留，不替代官方分类
- 统一 bucket 是辅助视角，不作为主分数来源
- 做 Memoria 公平评估时，要同时保留 retrieval 指标和 end-to-end QA 指标

## 六个统一能力桶

1. `Single-Session Grounding`
   - 单会话内的事实、上下文或显式信息提取

2. `Preference Understanding`
   - 用户偏好理解，不只是关键词命中

3. `Multi-Session Synthesis`
   - 跨会话整合、综合、总结

4. `Temporal State Tracking`
   - 时间、顺序、状态演化、事件先后

5. `Knowledge Update And Conflict Handling`
   - 新旧知识更新、冲突处理、矛盾消解

6. `Abstention And Constraint Following`
   - 无证据时拒答，以及记住并遵守约束/指令

## LongMemEval 映射

- `single-session-user` -> `Single-Session Grounding`
- `single-session-assistant` -> `Single-Session Grounding`
- `single-session-preference` -> `Preference Understanding`
- `multi-session` -> `Multi-Session Synthesis`
- `temporal-reasoning` -> `Temporal State Tracking`
- `knowledge-update` -> `Knowledge Update And Conflict Handling`
- `abstention` / `_abs` -> `Abstention And Constraint Following`

说明：

- 你之前说的 `single session`，在 LongMemEval 里其实是三类：
  `single-session-user`、`single-session-assistant`、`single-session-preference`
- `Preference Understanding Beyond Keyword Matching`
  对应 `single-session-preference`
- `Temporal State Tracking`
  对应 `temporal-reasoning`
- `Knowledge Evolution and Conflict Resolution`
  在 LongMemEval 里更接近 `knowledge-update`

## BEAM 映射

- `Information Extraction` -> `Single-Session Grounding`
- `Preference Following` -> `Preference Understanding`
- `Multi-Session Reasoning` -> `Multi-Session Synthesis`
- `Summarization` -> `Multi-Session Synthesis`
- `Temporal Reasoning` -> `Temporal State Tracking`
- `Event Ordering` -> `Temporal State Tracking`
- `Knowledge Update` -> `Knowledge Update And Conflict Handling`
- `Contradiction Resolution` -> `Knowledge Update And Conflict Handling`
- `Abstention` -> `Abstention And Constraint Following`
- `Instruction Following` -> `Abstention And Constraint Following`

说明：

- BEAM 没有官方 `single session` 桶
- 你说的 `Temporal State Tracking` 在 BEAM 里通常覆盖
  `Temporal Reasoning`，有时也会包含 `Event Ordering`
- 你说的 `Knowledge Evolution and Conflict Resolution` 在 BEAM 里应拆成
  `Knowledge Update` 和 `Contradiction Resolution`

## 为什么仍然保留这个辅助视角

4 个大桶太粗，会把一些关键能力压扁：

- LongMemEval `multi-session`
- LongMemEval `abstention`
- BEAM `instruction following`
- BEAM `summarization`
- BEAM `event ordering`

所以更适合：

- 对外主报告：按 LongMemEval / BEAM 官方分类分别展示
- 对内辅助分析：再看这 6 个 bucket 的统一视角

## 报告建议

对每个 benchmark 报告保留三层：

1. 官方原始标签分布
2. 可选的统一 6-bucket 汇总
3. 顶层 retrieval / QA / end-to-end 总分

这样做的好处：

- 便于 LongMemEval 和 BEAM 横向比较
- 不会丢失 benchmark 原始语义
- 更适合做 Memoria 的版本对比和回归跟踪

## 使用方式

对已有 report JSON 做归一化：

```bash
uv run python scripts/rollup_benchmark_report.py path/to/report.json
```

对一个结果目录递归处理：

```bash
uv run python scripts/rollup_benchmark_report.py benchmarks/results/
```

处理后，每个报告会新增：

- `memory_ability_taxonomy`
- `by_memory_ability_bucket`
- 每条 result 的
  - `memory_ability_bucket`
  - `memory_ability_bucket_label`
