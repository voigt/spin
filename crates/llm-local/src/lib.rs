mod bert;

use anyhow::Context;
use bert::{BertModel, Config};
use candle::DType;
use candle_nn::VarBuilder;
use llm::{
    InferenceFeedback, InferenceParameters, InferenceResponse, InferenceSessionConfig, Model,
    ModelKVMemoryType, ModelParameters,
};
use rand::SeedableRng;
use spin_core::async_trait;
use spin_llm::{model_arch, model_name, LlmEngine, MODEL_ALL_MINILM_L6_V2};
use spin_world::llm::{self as wasi_llm};
use std::{
    collections::hash_map::Entry,
    collections::HashMap,
    convert::Infallible,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokenizers::PaddingParams;

#[derive(Default)]
pub struct LLmOptions {
    pub use_gpu: bool,
}

#[derive(Clone)]
pub struct LocalLlmEngine {
    registry: PathBuf,
    use_gpu: bool,
    inferencing_models: HashMap<(String, bool), Arc<dyn llm::Model>>,
    embeddings_models: HashMap<String, Arc<(tokenizers::Tokenizer, BertModel)>>,
}

#[async_trait]
impl LlmEngine for LocalLlmEngine {
    async fn infer(
        &mut self,
        model: wasi_llm::InferencingModel,
        prompt: String,
        params: wasi_llm::InferencingParams,
    ) -> Result<wasi_llm::InferencingResult, wasi_llm::Error> {
        let model = self.inferencing_model(model).await?;
        let cfg = InferenceSessionConfig {
            memory_k_type: ModelKVMemoryType::Float16,
            memory_v_type: ModelKVMemoryType::Float16,
            n_batch: 8,
            n_threads: num_cpus::get(),
        };

        let mut session = Model::start_session(model.as_ref(), cfg);
        let inference_params = InferenceParameters {
            sampler: generate_sampler(params),
        };
        let mut rng = rand::rngs::StdRng::from_entropy();
        let mut text = String::new();

        #[cfg(debug_assertions)]
        {
            terminal::warn!(
                "\
                This is a debug build - running inference might be prohibitively slow\n\
                You may want to consider switching to the release build"
            )
        }
        let res = session.infer::<Infallible>(
            model.as_ref(),
            &mut rng,
            &llm::InferenceRequest {
                prompt: prompt.as_str().into(),
                parameters: &inference_params,
                play_back_previous_tokens: false,
                maximum_token_count: Some(params.max_tokens as usize),
            },
            &mut Default::default(),
            |r| {
                match r {
                    InferenceResponse::InferredToken(t) => text.push_str(&t),
                    InferenceResponse::EotToken => return Ok(InferenceFeedback::Halt),
                    _ => {}
                };
                Ok(InferenceFeedback::Continue)
            },
        );
        let stats = res.map_err(|e| {
            wasi_llm::Error::RuntimeError(format!("Error occurred during inferencing: {e}"))
        })?;
        let usage = wasi_llm::InferencingUsage {
            prompt_token_count: stats.prompt_tokens as u32,
            generated_token_count: (stats.predict_tokens - stats.prompt_tokens) as u32,
        };
        let response = wasi_llm::InferencingResult { text, usage };
        Ok(response)
    }

    async fn generate_embeddings(
        &mut self,
        model: wasi_llm::EmbeddingModel,
        data: Vec<String>,
    ) -> Result<wasi_llm::EmbeddingsResult, wasi_llm::Error> {
        let model = self.embeddings_model(model).await?;
        generate_embeddings(data, model).await.map_err(|e| {
            wasi_llm::Error::RuntimeError(format!("Error occurred generating embeddings: {e}"))
        })
    }
}

impl LocalLlmEngine {
    pub async fn new(registry: PathBuf, use_gpu: bool) -> Self {
        let mut engine = Self {
            registry,
            use_gpu,
            inferencing_models: Default::default(),
            embeddings_models: Default::default(),
        };

        let _ = engine.inferencing_model("llama2-chat".into()).await;
        let _ = engine.embeddings_model(MODEL_ALL_MINILM_L6_V2.into()).await;

        engine
    }

    /// Get embeddings model from cache or load from disk
    async fn embeddings_model(
        &mut self,
        model: wasi_llm::EmbeddingModel,
    ) -> Result<Arc<(tokenizers::Tokenizer, BertModel)>, wasi_llm::Error> {
        let key = match model.as_str() {
            MODEL_ALL_MINILM_L6_V2 => model,
            _ => return Err(wasi_llm::Error::ModelNotSupported),
        };
        let registry_path = self.registry.join(&key);
        let r = match self.embeddings_models.entry(key) {
            Entry::Occupied(o) => o.get().clone(),
            Entry::Vacant(v) => v
                .insert({
                    tokio::task::spawn_blocking(move || {
                        if !registry_path.exists() {
                            return Err(
                                wasi_llm::Error::RuntimeError(format!(
                                "The directory expected to house the embeddings models '{}' does not exist.",
                                registry_path.display()
                            )));
                        }
                        let tokenizer_file = registry_path.join("tokenizer.json");
                        let model_file = registry_path.join("model.safetensors");
                        let tokenizer = load_tokenizer(&tokenizer_file).map_err(|_| {
                            wasi_llm::Error::RuntimeError(format!(
                                "Failed to load embeddings tokenizer from '{}'",
                                tokenizer_file.display()
                            ))
                        })?;
                        let model = load_model(&model_file).map_err(|_| {
                            wasi_llm::Error::RuntimeError(format!(
                                "Failed to load embeddings model from '{}'",
                                model_file.display()
                            ))
                        })?;
                        Ok(Arc::new((tokenizer, model)))
                    })
                    .await
                    .map_err(|_| {
                        wasi_llm::Error::RuntimeError("Error loading inferencing model".into())
                    })??
                })
                .clone(),
        };
        Ok(r)
    }

    /// Get inferencing model from cache or load from disk
    async fn inferencing_model(
        &mut self,
        model: wasi_llm::InferencingModel,
    ) -> Result<Arc<dyn Model>, wasi_llm::Error> {
        let model_name = model_name(&model)?;
        let use_gpu = self.use_gpu;
        let progress_fn = |_| {};
        let model = match self.inferencing_models.entry((model_name.into(), use_gpu)) {
            Entry::Occupied(o) => o.get().clone(),
            Entry::Vacant(v) => v
                .insert({
                    let path = self.registry.join(model_name);
                    if !self.registry.exists() {
                        return Err(wasi_llm::Error::RuntimeError(
                            format!("The directory expected to house the inferencing model '{}' does not exist.", self.registry.display())
                        ));
                    }
                    if !path.exists() {
                        return Err(wasi_llm::Error::RuntimeError(
                            format!("The inferencing model file '{}' does not exist.", path.display())
                        ));
                    }
                    tokio::task::spawn_blocking(move || {
                        let params = ModelParameters {
                            prefer_mmap: true,
                            context_size: 2048,
                            lora_adapters: None,
                            use_gpu,
                            gpu_layers: None,
                            rope_overrides: None,
                            n_gqa: None,
                        };
                        let model = llm::load_dynamic(
                            Some(model_arch(&model)?),
                            &path,
                            llm::TokenizerSource::Embedded,
                            params,
                            progress_fn,
                        )
                        .map_err(|e| {
                            wasi_llm::Error::RuntimeError(format!(
                                "Failed to load model from model registry: {e}"
                            ))
                        })?;
                        Ok(Arc::from(model))
                    })
                    .await
                    .map_err(|_| {
                        wasi_llm::Error::RuntimeError("Error loading inferencing model".into())
                    })??
                })
                .clone(),
        };
        Ok(model)
    }
}

async fn generate_embeddings(
    data: Vec<String>,
    model: Arc<(tokenizers::Tokenizer, BertModel)>,
) -> anyhow::Result<wasi_llm::EmbeddingsResult> {
    let n_sentences = data.len();
    tokio::task::spawn_blocking(move || {
        let mut tokenizer = model.0.clone();
        let model = &model.1;
        // This function attempts to generate the embeddings for a batch of inputs, most
        // likely of different lengths.
        // The tokenizer expects all inputs in a batch to have the same length, so the
        // following is configuring the tokenizer to pad (add trailing zeros) each input
        // to match the length of the longest in the batch.
        if let Some(pp) = tokenizer.get_padding_mut() {
            pp.strategy = tokenizers::PaddingStrategy::BatchLongest
        } else {
            let pp = PaddingParams {
                strategy: tokenizers::PaddingStrategy::BatchLongest,
                ..Default::default()
            };
            tokenizer.with_padding(Some(pp));
        }
        let tokens = tokenizer
            .encode_batch(data, true)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let token_ids = tokens
            .iter()
            .map(|tokens| {
                let tokens = tokens.get_ids().to_vec();
                Ok(candle::Tensor::new(
                    tokens.as_slice(),
                    &candle::Device::Cpu,
                )?)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        // Execute the model's forward propagation function, which generates the raw embeddings.
        let token_ids = candle::Tensor::stack(&token_ids, 0)?;
        let embeddings = model.forward(&token_ids, &token_ids.zeros_like()?)?;

        // SBERT adds a pooling operation to the raw output to derive a fixed sized sentence embedding.
        // The BERT models suggest using mean pooling, which is what the operation below performs.
        // https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2#usage-huggingface-transformers
        let (_, n_tokens, _) = embeddings.dims3()?;
        let embeddings = (embeddings.sum(1)? / (n_tokens as f64))?;

        // Take each sentence embedding from the batch and arrange it in the final result tensor.
        // Normalize each embedding as the last step (this generates vectors with length 1, which
        // makes the cosine similarity function significantly more efficient (it becomes a simple
        // dot product).
        let mut results: Vec<Vec<f32>> = Vec::new();
        for j in 0..n_sentences {
            let e_j = embeddings.get(j)?;
            let mut emb: Vec<f32> = e_j.to_vec1()?;
            let length: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
            emb.iter_mut().for_each(|x| *x /= length);
            results.push(emb);
        }

        let result = wasi_llm::EmbeddingsResult {
            embeddings: results,
            usage: wasi_llm::EmbeddingsUsage {
                prompt_token_count: n_tokens as u32,
            },
        };
        Ok(result)
    })
    .await?
}

fn load_tokenizer(tokenizer_file: &Path) -> anyhow::Result<tokenizers::Tokenizer> {
    let tokenizer = tokenizers::Tokenizer::from_file(tokenizer_file)
        .map_err(|e| anyhow::anyhow!("Failed to read tokenizer file {tokenizer_file:?}: {e}"))?;
    Ok(tokenizer)
}

fn load_model(model_file: &Path) -> anyhow::Result<BertModel> {
    let buffer = std::fs::read(model_file)
        .with_context(|| format!("Failed to read model file {model_file:?}"))?;
    let weights = safetensors::SafeTensors::deserialize(&buffer)?;
    let vb = VarBuilder::from_safetensors(vec![weights], DType::F32, &candle::Device::Cpu);
    let model = BertModel::load(vb, &Config::default()).context("error loading bert model")?;
    Ok(model)
}

// Sampling options for picking the next token in the sequence.
// We start with a default sampler, then add the inference parameters supplied by the request.
fn generate_sampler(
    params: wasi_llm::InferencingParams,
) -> Arc<Mutex<dyn llm::samplers::llm_samplers::types::Sampler<llm::TokenId, f32>>> {
    let mut result = llm::samplers::ConfiguredSamplers {
        // We are *not* using the default implementation for ConfiguredSamplers here
        // because the builder already sets values for parameters, which we cannot replace.
        builder: llm::samplers::llm_samplers::configure::SamplerChainBuilder::default(),
        ..Default::default()
    };

    result.builder += (
        "temperature".into(),
        llm::samplers::llm_samplers::configure::SamplerSlot::new_single(
            move || {
                Box::new(
                    llm::samplers::llm_samplers::samplers::SampleTemperature::default()
                        .temperature(params.temperature),
                )
            },
            Option::<llm::samplers::llm_samplers::samplers::SampleTemperature>::None,
        ),
    );
    result.builder += (
        "topp".into(),
        llm::samplers::llm_samplers::configure::SamplerSlot::new_single(
            move || {
                Box::new(
                    llm::samplers::llm_samplers::samplers::SampleTopP::default().p(params.top_p),
                )
            },
            Option::<llm::samplers::llm_samplers::samplers::SampleTopP>::None,
        ),
    );
    result.builder += (
        "topk".into(),
        llm::samplers::llm_samplers::configure::SamplerSlot::new_single(
            move || {
                Box::new(
                    llm::samplers::llm_samplers::samplers::SampleTopK::default()
                        .k(params.top_k as usize),
                )
            },
            Option::<llm::samplers::llm_samplers::samplers::SampleTopK>::None,
        ),
    );
    result.builder += (
        "repetition".into(),
        llm::samplers::llm_samplers::configure::SamplerSlot::new_chain(
            move || {
                Box::new(
                    llm::samplers::llm_samplers::samplers::SampleRepetition::default()
                        .penalty(params.repeat_penalty)
                        .last_n(params.repeat_penalty_last_n_token_count as usize),
                )
            },
            [],
        ),
    );

    result.ensure_default_slots();
    Arc::new(Mutex::new(result.builder.into_chain()))
}
