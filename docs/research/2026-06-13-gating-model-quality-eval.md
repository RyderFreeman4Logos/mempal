# mempal 历史记忆 gating 模型质量评估（最终 aggregate）

日期：2026-06-13 / 2026-06-14 UTC artifact

> 本报告只记录 aggregate 指标。评估过程不执行 forget，不修改运行库，不输出原始记忆内容、prompt、模型原文回答、API key/token 或连接串。
>
> 说明：当前仓库 `main` 原本没有 `docs/research/` 文件；本文件基于本地分支 `docs/gating-model-eval-research` 的中期报告更新，并加入 Qwen n=1000 完成结果。

## 结论摘要

本次评估比较 `spark` 与 `qwen3.6-27b-decensor-by-aeon` 作为历史记忆 gating judge 的保留/拒绝倾向。

核心结论：

1. **两者都不适合作为单模型 destructive forget 最终授权。** 相对 pseudo-gold，Spark 与 Qwen 都有较高误差；LLM judge 失败或 missing 必须 fail-safe keep/no-delete。
2. **Qwen n=1000 比 Spark n=1000 更会拒绝 pseudo-reject 垃圾样本**：Qwen FPR（pseudo reject 被 keep）为 **28.23%**，Spark n=1000 为 **53.94%**。
3. **Qwen 的 deletion-risk 指标更差**：Qwen FNR（pseudo keep 被 reject）为 **53.35%**，Spark n=1000 为 **42.34%**。如果把模型 reject 直接映射为删除，Qwen 在 pseudo keep 上误拒更多。
4. **Spark 更快但更容易保留垃圾；Qwen 更慢且更保守/挑剔。** Spark batch=20 平均 batch latency 约 7s；Qwen batch=2 平均 batch latency 约 72s，且中途有 RemoteDisconnected/Timeout 聚集。
5. **推荐策略**：真实 forget 只能继续走可回滚 soft-delete + backup + checkpoint + restore/resume 证据链；LLM 只能作为候选排序或 veto/二级信号，不可单独授权删除。

## 数据与标签定义

样本来自本机 `~/.mempal/palace.db` 的只读扫描，优先覆盖早期从 claude-mem / agent 历史导入、缺少初始 gating 的记忆候选。报告只记录 aggregate。

### pseudo-gold 标签

当前评估没有人工标注真值，因此使用高置信 pseudo-gold：

- `keep`：已有 keep audit、语义 room + durable keyword、或高密度 durable signal。
- `reject`：明显工具输出、日志/diff、路径/文件列表、JSON tool dump、太短/低信号、结构化代码 dump。
- ambiguous/unlabeled：不纳入 FP/FN 统计。

因此本文的 FP/FN 是“相对于 pseudo-gold”的误差率，不等同人工真值误差率。它适合比较模型倾向和找风险，不适合作为无复核删除授权。

### 全量候选扫描

- scanned：518666
- pseudo-labeled keep：100478
- pseudo-labeled reject：7747
- ambiguous/unlabeled：410346
- ambiguous_has_keep_and_neg：95

### 主样本 n=2000

- by_label：keep 1000，reject 1000
- by_source_type：agent_inference 1684，system_generated 311，agent_observation 5
- positive strata：existing_gating_keep 334，keyword_dense_durable_signal 334，semantic_room_keyword 332
- negative strata：path_heavy_tool_output 487，raw_log_or_diff 338，tier1_too_short 141，raw_file_listing_or_read_output 28，tier1_boilerplate 3，json_tool_dump 2，code_or_structured_dump 1
- content length：p50=767，p90=799，p99=49495，max=64413
- evaluation representation：每条最多 600 chars；representation p50=639，p90=639，max=641

### 同批前缀 n=1000

Spark n=1000 与 Qwen n=1000 使用同一批 n=2000 样本的前缀。

- by_label：keep 473，reject 527
- by_source_type：agent_inference 839，system_generated 158，agent_observation 3
- positive strata：existing_gating_keep 154，keyword_dense_durable_signal 141，semantic_room_keyword 178
- negative strata：path_heavy_tool_output 259，raw_log_or_diff 173，tier1_too_short 72，raw_file_listing_or_read_output 18，tier1_boilerplate 3，json_tool_dump 1，code_or_structured_dump 1
- content length：p50=767，p90=799，p99=51273，max=61612
- evaluation representation：每条最多 600 chars；representation p50=639，p90=639，p99=641，max=641

## Prompt / 推理设置

两个模型使用同一判定任务：输出 JSON，逐条给出 `keep|reject`、confidence、reason_category；提示要求不要引用或泄露记忆内容。

共同设置：

- temperature：0
- max_content_chars：600
- max_tokens：36000
- timeout：900s
- 输出保存为 aggregate；报告不包含原始记忆、prompt 内容或模型原文回答。

模型特定设置：

- `spark`：batch_size=20，请求携带 `reasoning_effort=high`、`thinking_token_budget=32768`；usage 中报告 reasoning tokens。
- `qwen3.6-27b-decensor-by-aeon`：batch_size=2，服务端 high / 32768 thinking budget；usage 未报告 reasoning tokens。

## 结果总表

| run | sample_n | n_decided | missing | accuracy vs pseudo-gold | FPR: pseudo reject→keep | FNR: pseudo keep→reject | mean batch latency |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Spark n=2000 | 2000 | 1946 | 54 | 52.31% | 52.37% `[49.22%, 55.50%]` | 43.03% `[39.96%, 46.16%]` | 7.26s |
| Spark n=1000 prefix | 1000 | 939 | 61 | 51.54% | 53.94% `[49.53%, 58.28%]` | 42.34% `[37.83%, 46.98%]` | 7.75s |
| Qwen n=1000 prefix | 1000 | 951 | 49 | 59.94% | 28.23% `[24.47%, 32.32%]` | 53.35% `[48.72%, 57.92%]` | 71.95s |

Wilson 95% CI shown for FPR/FNR.

## Spark 结果

### Spark n=2000

可解析判定 1946 / 2000，missing 54。

Confusion matrix（相对于 pseudo-gold）：

| pseudo-gold | pred keep | pred reject |
| --- | ---: | ---: |
| keep | 556 | 420 |
| reject | 508 | 462 |

误差率：

- FPR（pseudo reject 被 keep）：508 / 970 = **52.37%**，Wilson 95% CI `[49.22%, 55.50%]`
- FNR（pseudo keep 被 reject）：420 / 976 = **43.03%**，Wilson 95% CI `[39.96%, 46.16%]`
- accuracy vs pseudo-gold：52.31%
- keep precision / recall：52.26% / 56.97%
- reject precision / recall：52.38% / 47.63%

运行质量：

- failures：missing_items_in_batch=54，parse_error=1
- batch latency：mean 7.26s，p50 6.47s，p90 10.16s，max 31.45s（100 batches）
- completion tokens：mean 6423，p50 6212，p90 8387，max 11724
- prompt tokens：mean 3470，p50 3438，p90 3911，max 4312
- reasoning tokens（reported）：mean 5826，p50 5632，p90 7773

### Spark n=1000 同批前缀

可解析判定 939 / 1000，missing 61。

Confusion matrix：

| pseudo-gold | pred keep | pred reject |
| --- | ---: | ---: |
| keep | 256 | 188 |
| reject | 267 | 228 |

误差率：

- FPR：267 / 495 = **53.94%**，Wilson 95% CI `[49.53%, 58.28%]`
- FNR：188 / 444 = **42.34%**，Wilson 95% CI `[37.83%, 46.98%]`
- accuracy vs pseudo-gold：51.54%

运行质量：

- failures：http_408=1，http_503=3，missing_items_in_batch=21
- batch latency：mean 7.75s，p50 6.14s，p90 10.81s，max 36.32s（48 batches）

## Qwen 结果

### Qwen n=1000 同批前缀

可解析判定 951 / 1000，missing 49。

Confusion matrix（相对于 pseudo-gold）：

| pseudo-gold | pred keep | pred reject |
| --- | ---: | ---: |
| keep | 209 | 239 |
| reject | 142 | 361 |

误差率：

- FPR（pseudo reject 被 keep）：142 / 503 = **28.23%**，Wilson 95% CI `[24.47%, 32.32%]`
- FNR（pseudo keep 被 reject）：239 / 448 = **53.35%**，Wilson 95% CI `[48.72%, 57.92%]`
- accuracy vs pseudo-gold：59.94%
- keep precision / recall：59.54% / 46.65%
- reject precision / recall：60.17% / 71.77%

运行质量：

- failures：RemoteDisconnected=46，TimeoutError=2，missing_items_in_batch=1
- batch latency：mean 71.95s，p50 69.36s，p90 95.55s，max 889.34s（476 batches）
- completion tokens：mean 1525，p50 1515，p90 2050，max 3301
- prompt tokens：mean 541，p50 555，p90 677，max 858
- reasoning tokens：服务未在 usage 中报告

## 解读与对 forget 的影响

### 模型倾向

- Spark：更接近随机/混合，保留 pseudo reject 的比例很高，说明它不擅长把工具输出、日志/diff、路径/文件列表等垃圾内容稳定拒绝。
- Qwen：更擅长拒绝 pseudo reject，但也更容易把 pseudo keep 判成 reject。它像一个更 aggressive 的垃圾识别器，而不是安全删除授权器。

### 删除安全

若把 `pred reject` 映射为“删除候选”，FNR（pseudo keep 被 reject）就是核心风险指标。该指标：

- Spark n=1000：42.34%
- Qwen n=1000：53.35%

因此 Qwen 虽然减少了“垃圾被保留”，但增加了“应保留内容被拒绝”的风险。真实 forget 不能让 Qwen 单模型直接授权删除。

### 推荐策略

1. 继续执行 **可回滚 soft-delete**，不得 hard-delete。
2. LLM judge missing / parse error / timeout / endpoint failure 一律 fail-safe keep/no-delete。
3. 删除候选应依赖 deterministic/规则层的高置信信号；LLM 更适合做：
   - 候选排序；
   - 高风险样本 veto；
   - 二级复核队列；
   - aggregate policy tuning，而不是单点授权。
4. 对于模型分歧：
   - Spark keep + Qwen reject：需要特别谨慎，不能直接删除；
   - Spark reject + Qwen reject 且规则层也 reject：可作为较高置信 soft-delete 候选；
   - 任一模型失败/missing：keep。
5. 当前 #427 已修复 transient LLM timeout 导致 historical rejudge abort 的问题；恢复 forget 时应继续从 checkpoint resume，并验证 progress/report aggregate。

## Artifact 位置

本地 aggregate artifact：

- Spark n=2000：`/tmp/mempal_gating_eval_spark_n2000.json`
- Spark n=1000 prefix：`/tmp/mempal_gating_eval_spark_n1000.json`
- Qwen n=1000 prefix：`/tmp/mempal_gating_eval_qwen_n1000_b2.json`

这些 artifact 是本地 aggregate 输出；报告没有包含原始样本内容或模型原文回答。

## 后续建议

1. 若要进一步降低误删风险，应抽样人工标注一小批 pseudo keep / pseudo reject 边界样本，校准 pseudo-gold 偏差。
2. 将 model judge 结果保留为 audit signal，而不是删除真值。
3. 在当前 Spark forget resume 中持续只看 aggregate progress/report；若失败，优先检查 checkpoint、backup、DB lock、endpoint cooldown，而不是重跑 hard-delete 或从头开始。
