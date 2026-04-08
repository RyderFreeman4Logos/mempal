use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use ort::{session::Session, session::SessionOutputs, value::Tensor};
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};

use crate::{EMBEDDING_DIMENSIONS, Embedder};

const MODEL_URL: &str =
    "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/onnx/model.onnx";
const TOKENIZER_URL: &str =
    "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/tokenizer.json";
const MODEL_FILE_NAME: &str = "all-MiniLM-L6-v2.onnx";
const TOKENIZER_FILE_NAME: &str = "all-MiniLM-L6-v2-tokenizer.json";
const MAX_SEQUENCE_LENGTH: usize = 256;
const PAD_TOKEN: &str = "[PAD]";
const PAD_TOKEN_ID: u32 = 0;

#[derive(Debug)]
pub struct OnnxEmbedder {
    session: Arc<Mutex<Session>>,
    tokenizer: Arc<Mutex<Tokenizer>>,
}

impl OnnxEmbedder {
    pub async fn new_or_download() -> Result<Self> {
        Self::new_or_download_in(default_models_dir()).await
    }

    pub async fn new_or_download_in(path: impl AsRef<Path>) -> Result<Self> {
        let models_dir = path.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&models_dir)
            .await
            .with_context(|| format!("failed to create model dir {}", models_dir.display()))?;

        let model_path = models_dir.join(MODEL_FILE_NAME);
        let tokenizer_path = models_dir.join(TOKENIZER_FILE_NAME);

        download_if_missing(&model_path, MODEL_URL).await?;
        download_if_missing(&tokenizer_path, TOKENIZER_URL).await?;

        let session = Session::builder()
            .context("failed to create ONNX session builder")?
            .commit_from_file(&model_path)
            .with_context(|| format!("failed to load ONNX model from {}", model_path.display()))?;
        let tokenizer = load_tokenizer(&tokenizer_path)?;

        Ok(Self {
            session: Arc::new(Mutex::new(session)),
            tokenizer: Arc::new(Mutex::new(tokenizer)),
        })
    }
}

#[async_trait::async_trait]
impl Embedder for OnnxEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let owned_texts = texts
            .iter()
            .map(|text| (*text).to_string())
            .collect::<Vec<_>>();
        let tokenizer = Arc::clone(&self.tokenizer);
        let session = Arc::clone(&self.session);

        tokio::task::spawn_blocking(move || {
            let tokenizer = tokenizer
                .lock()
                .map_err(|_| anyhow!("tokenizer mutex poisoned"))?;
            let encoded = tokenizer
                .encode_batch(owned_texts, true)
                .map_err(|error| anyhow!(error.to_string()))?;
            drop(tokenizer);

            let batch_size = encoded.len();
            let sequence_length = encoded
                .first()
                .map(|encoding| encoding.len())
                .context("tokenizer returned no encodings for non-empty input")?;

            let input_ids = encoded
                .iter()
                .flat_map(|encoding| encoding.get_ids().iter().map(|id| i64::from(*id)))
                .collect::<Vec<_>>();
            let attention_mask = encoded
                .iter()
                .flat_map(|encoding| {
                    encoding
                        .get_attention_mask()
                        .iter()
                        .map(|mask| i64::from(*mask))
                })
                .collect::<Vec<_>>();
            let token_type_ids = encoded
                .iter()
                .flat_map(|encoding| {
                    encoding
                        .get_type_ids()
                        .iter()
                        .map(|type_id| i64::from(*type_id))
                })
                .collect::<Vec<_>>();

            let mut session = session
                .lock()
                .map_err(|_| anyhow!("onnx session mutex poisoned"))?;
            let expects_token_type_ids = session
                .inputs()
                .iter()
                .any(|input| input.name() == "token_type_ids");

            let mut inputs: Vec<(&str, ort::session::SessionInputValue<'_>)> = vec![
                (
                    "input_ids",
                    Tensor::<i64>::from_array((
                        [batch_size, sequence_length],
                        input_ids.into_boxed_slice(),
                    ))?
                    .into(),
                ),
                (
                    "attention_mask",
                    Tensor::<i64>::from_array((
                        [batch_size, sequence_length],
                        attention_mask.clone().into_boxed_slice(),
                    ))?
                    .into(),
                ),
            ];

            if expects_token_type_ids {
                inputs.push((
                    "token_type_ids",
                    Tensor::<i64>::from_array((
                        [batch_size, sequence_length],
                        token_type_ids.into_boxed_slice(),
                    ))?
                    .into(),
                ));
            }

            let outputs = session
                .run(inputs)
                .context("failed to run ONNX embedding session")?;

            extract_embeddings(outputs, &attention_mask, sequence_length)
        })
        .await
        .context("onnx embedding worker panicked")?
    }

    fn dimensions(&self) -> usize {
        EMBEDDING_DIMENSIONS
    }

    fn name(&self) -> &str {
        "onnx"
    }
}

async fn download_if_missing(path: &Path, url: &str) -> Result<()> {
    if tokio::fs::try_exists(path)
        .await
        .with_context(|| format!("failed to check whether {} exists", path.display()))?
    {
        return Ok(());
    }

    let response = reqwest::get(url)
        .await
        .with_context(|| format!("failed to download {url}"))?
        .error_for_status()
        .with_context(|| format!("download returned error status for {url}"))?;
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("failed to read download body from {url}"))?;

    tokio::fs::write(path, &bytes)
        .await
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(())
}

fn load_tokenizer(path: &Path) -> Result<Tokenizer> {
    let mut tokenizer = Tokenizer::from_file(path).map_err(|error| anyhow!(error.to_string()))?;
    let pad_id = tokenizer.token_to_id(PAD_TOKEN).unwrap_or(PAD_TOKEN_ID);

    tokenizer.with_padding(Some(PaddingParams {
        pad_id,
        pad_token: PAD_TOKEN.to_string(),
        ..PaddingParams::default()
    }));
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: MAX_SEQUENCE_LENGTH,
            ..TruncationParams::default()
        }))
        .map_err(|error| anyhow!(error.to_string()))?;

    Ok(tokenizer)
}

fn extract_embeddings(
    outputs: SessionOutputs<'_>,
    attention_mask: &[i64],
    sequence_length: usize,
) -> Result<Vec<Vec<f32>>> {
    if outputs.contains_key("sentence_embedding") {
        return collect_2d_embeddings(&outputs["sentence_embedding"]);
    }

    if outputs.contains_key("embeddings") {
        return collect_2d_embeddings(&outputs["embeddings"]);
    }

    let output = &outputs[0];
    let array = output.try_extract_array::<f32>()?;
    let shape = array.shape().to_vec();
    let data = array.iter().copied().collect::<Vec<_>>();

    match shape.as_slice() {
        [batch_size, dim] => {
            if *dim != EMBEDDING_DIMENSIONS {
                bail!("unexpected embedding dimension: {dim}");
            }

            Ok(data
                .chunks(*dim)
                .take(*batch_size)
                .map(|chunk| chunk.to_vec())
                .collect())
        }
        [batch_size, tokens, dim] => {
            if *dim != EMBEDDING_DIMENSIONS {
                bail!("unexpected token embedding dimension: {dim}");
            }
            if *tokens != sequence_length {
                bail!(
                    "token embedding length mismatch: model returned {tokens}, tokenizer produced {sequence_length}"
                );
            }

            let mut results = Vec::with_capacity(*batch_size);
            for batch_index in 0..*batch_size {
                let mut pooled = vec![0.0_f32; *dim];
                let mut token_count = 0.0_f32;

                for token_index in 0..*tokens {
                    let mask_index = batch_index * sequence_length + token_index;
                    if attention_mask.get(mask_index).copied().unwrap_or_default() == 0 {
                        continue;
                    }

                    let offset = (batch_index * tokens * dim) + (token_index * dim);
                    for dimension in 0..*dim {
                        pooled[dimension] += data[offset + dimension];
                    }
                    token_count += 1.0;
                }

                if token_count == 0.0 {
                    bail!("encountered sequence with no attended tokens");
                }

                for value in &mut pooled {
                    *value /= token_count;
                }
                l2_normalize(&mut pooled);
                results.push(pooled);
            }

            Ok(results)
        }
        _ => bail!("unexpected ONNX output shape: {shape:?}"),
    }
}

fn collect_2d_embeddings(value: &ort::value::DynValue) -> Result<Vec<Vec<f32>>> {
    let array = value.try_extract_array::<f32>()?;
    let shape = array.shape().to_vec();
    let data = array.iter().copied().collect::<Vec<_>>();

    match shape.as_slice() {
        [batch_size, dim] => {
            if *dim != EMBEDDING_DIMENSIONS {
                bail!("unexpected embedding dimension: {dim}");
            }

            Ok(data
                .chunks(*dim)
                .take(*batch_size)
                .map(|chunk| chunk.to_vec())
                .collect())
        }
        _ => bail!("expected 2D embedding output, got {shape:?}"),
    }
}

fn l2_normalize(values: &mut [f32]) {
    let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in values {
            *value /= norm;
        }
    }
}

fn default_models_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".mempal").join("models"))
        .unwrap_or_else(|| PathBuf::from(".mempal/models"))
}
