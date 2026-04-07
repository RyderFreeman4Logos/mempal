spec: task
name: "P3: AAAK codec — BNF grammar, encoder, decoder, roundtrip verification"
inherits: project
tags: [p3, aaak, codec]
depends: [p0-core-scaffold]
estimate: 2d
---

## Intent

实现 mempal-aaak crate：完整的 AAAK 编解码器，包含形式语法（BNF）、编码器、解码器和往返验证。
修复 MemPalace 中 AAAK 的核心缺陷：没有解码器、没有往返测试、压缩是有损的。
AAAK 只在输出侧使用（wake-up --format aaak），不影响存储和检索。

## Decisions

- 编码器 + 解码器必须成对实现
- 形式语法：BNF 定义，代码中用 `nom` 或手写 parser 实现
- 实体编码：BiMap 双向映射（`bimap` crate）
- 情感编码：28 种情感 → 3-7 字符短编码
- 语义标志：DECISION / ORIGIN / CORE / PIVOT / TECHNICAL / SENSITIVE
- 往返验证：`verify_roundtrip()` 返回 RoundtripReport（preserved / lost / coverage）
- 覆盖率阈值：编码器生成时报告覆盖率，不做静默丢弃
- 格式版本：V1，头部行包含版本号
- 截断行为透明：如果 key_sentence 或 topics 被截断，报告截断的事实

## Boundaries

### Allowed Changes
- crates/mempal-aaak/**
- crates/mempal-cli/**（添加 `wake-up --format aaak` 和 `compress` 命令）

### Forbidden
- 不修改 mempal-ingest（AAAK 不在存储路径上）
- 不修改 mempal-search（AAAK 不在检索路径上）
- 不使用 LLM 做编码或解码

## Completion Criteria

Scenario: 编码基本文本
  Test: test_aaak_encode
  Given 文本 "Kai recommended Clerk over Auth0 based on pricing and DX"
  When 调用 `codec.encode(text, meta)`
  Then 返回 AaakDocument 包含实体编码（KAI）和 DECISION 标志

Scenario: 解码还原
  Test: test_aaak_decode
  Given 一个 AaakDocument
  When 调用 `codec.decode(doc)`
  Then 返回包含原始实体名和关系的文本

Scenario: 往返验证通过
  Test: test_aaak_roundtrip
  Given 文本包含 5 个事实断言
  When 编码后调用 `verify_roundtrip(original, encoded)`
  Then RoundtripReport.coverage >= 0.8
  And RoundtripReport.lost 列出未保留的断言

Scenario: 实体双向映射
  Test: test_entity_bimap
  Given entity_map 包含 ("Alice", "ALC")
  When encode 时遇到 "Alice"
  Then 编码为 "ALC"
  When decode 时遇到 "ALC"
  Then 还原为 "Alice"

Scenario: BNF 语法解析
  Test: test_aaak_parse
  Given 合法的 AAAK 字符串 `V1|myapp|auth|2026-04-08|readme\n0:KAI|clerk_auth|"use Clerk"|★★★★|determ|DECISION`
  When 调用 `AaakDocument::parse(input)`
  Then 解析成功，包含 header 和 1 个 zettel

Scenario: 非法 AAAK 字符串拒绝
  Test: test_aaak_parse_invalid
  Given 非法字符串 "this is not aaak"
  When 调用 `AaakDocument::parse(input)`
  Then 返回 ParseError

Scenario: 格式版本标记
  Test: test_aaak_version
  Given 任意编码输出
  When 检查头部行
  Then 以 "V1|" 开头

Scenario: 截断透明报告
  Test: test_aaak_truncation_report
  Given 一个包含 10 个 topics 的长文本
  When 编码（topics 限制 top-3）
  Then 返回的 EncodeReport 标注 topics_truncated=7
