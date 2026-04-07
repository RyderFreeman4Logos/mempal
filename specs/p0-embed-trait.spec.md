spec: task
name: "P0: Embedder trait + ONNX implementation"
inherits: project
tags: [p0, embed, onnx]
depends: [p0-core-scaffold]
estimate: 1d
---

## Intent

实现 mempal-embed crate：定义 `Embedder` trait 抽象层，并实现 `OnnxEmbedder`（使用 ort crate
加载 MiniLM ONNX 模型）。这是导入和搜索的前提——没有嵌入能力，无法生成向量。

## Decisions

- `Embedder` trait：`async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>`
- `OnnxEmbedder`：使用 `ort` crate 加载 `all-MiniLM-L6-v2` ONNX 模型
- 模型文件：首次运行时从 HuggingFace 下载到 `~/.mempal/models/`，之后离线使用
- 向量维度：384（MiniLM 默认）
- `ApiEmbedder`：占位实现，接受 endpoint + model 配置，调用 OpenAI 兼容 API

## Boundaries

### Allowed Changes
- crates/mempal-embed/**

### Forbidden
- 不修改 mempal-core 的类型定义
- 不实现 tokenizer（ort 内部处理）

## Completion Criteria

Scenario: ONNX 嵌入生成
  Test: test_onnx_embed
  Given OnnxEmbedder 已加载模型
  When 调用 `embed(&["hello world"])`
  Then 返回 1 个 384 维向量
  And 向量值全部在 -1.0 到 1.0 范围内

Scenario: 批量嵌入
  Test: test_onnx_batch
  Given OnnxEmbedder 已加载模型
  When 调用 `embed(&["text a", "text b", "text c"])`
  Then 返回 3 个 384 维向量

Scenario: 空输入
  Test: test_embed_empty
  Given 任意 Embedder
  When 调用 `embed(&[])`
  Then 返回空 Vec

Scenario: 模型文件不存在时下载
  Test: test_model_download
  Given 模型文件不存在
  When 创建 OnnxEmbedder
  Then 下载模型到 `~/.mempal/models/`
  And 后续创建不再下载

Scenario: API embedder 占位
  Test: test_api_embedder_config
  Given ApiEmbedder 配置了 endpoint 和 model
  When 检查 dimensions()
  Then 返回配置的维度值
