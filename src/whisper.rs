use std::fmt::Display;

use log::{info, warn};
use serde::Deserialize;
use whisper_rs::{
    DtwParameters, FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters,
    WhisperError,
};

use crate::util::resample;

#[derive(Debug)]
pub enum ErrSetupWhisper {
    WhisperError(WhisperError),
    IoError(std::io::Error),
    ReqwestError(reqwest::Error),
}

impl Display for ErrSetupWhisper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WhisperError(whisper_error) => write!(f, "{}", whisper_error),
            Self::IoError(io_error) => write!(f, "{}", io_error),
            Self::ReqwestError(reqwest_error) => write!(f, "{}", reqwest_error),
        }
    }
}

impl std::error::Error for ErrSetupWhisper {}

impl From<WhisperError> for ErrSetupWhisper {
    fn from(value: WhisperError) -> Self {
        Self::WhisperError(value)
    }
}

impl From<std::io::Error> for ErrSetupWhisper {
    fn from(value: std::io::Error) -> Self {
        Self::IoError(value)
    }
}

impl From<reqwest::Error> for ErrSetupWhisper {
    fn from(value: reqwest::Error) -> Self {
        Self::ReqwestError(value)
    }
}

#[derive(Debug)]
pub enum ErrTranscribe {
    WhisperError(WhisperError),
    ResampleError(speexdsp_resampler::Error),
}

impl Display for ErrTranscribe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WhisperError(whisper_error) => write!(f, "{}", whisper_error),
            Self::ResampleError(resample_error) =>
            // Speexdsp error isn't a real error >:(
            // https://github.com/rust-av/speexdsp-rs/issues/103
            {
                write!(f, "{:?}", resample_error)
            }
        }
    }
}

impl std::error::Error for ErrTranscribe {}

impl From<WhisperError> for ErrTranscribe {
    fn from(value: WhisperError) -> Self {
        Self::WhisperError(value)
    }
}

impl From<speexdsp_resampler::Error> for ErrTranscribe {
    fn from(value: speexdsp_resampler::Error) -> Self {
        Self::ResampleError(value)
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct WhisperConfig {
    pub model: String,
    pub language: Option<String>,
    pub translate: bool,
    pub no_context: bool,
    pub silence_length: u32, // Silence length in multiples of 21.3333ms
}

// Load whisper
pub fn setup_whisper(config: WhisperConfig) -> Result<WhisperContext, ErrSetupWhisper> {
    // Tell whisper to use log
    whisper_rs::install_logging_hooks();

    // Get relative path
    let model_path = format!("whisper/ggml-{}.bin", config.model);

    // Ensure whisper directory exists
    if let Ok(_) = std::fs::create_dir("whisper") {
        warn!("Whisper directory didnt exist, creating now");
    }

    // Check model exists
    if !std::fs::exists(&model_path)? {
        warn!("Model {} not found, attempting to download", model_path);

        // Construct url
        let url = format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{}.bin?download=true",
            config.model
        );

        // Create model file
        let mut model_file = std::fs::File::create(&model_path)?;

        // Download model file
        // TODO: Add a progress bar
        // TODO: Customise error type to explain model download
        let mut download = reqwest::blocking::get(url)?;

        // Copy contents
        std::io::copy(&mut download, &mut model_file)?;

        info!("Model {} downloaded", config.model);
    }

    // Create the context and load the model
    Ok(WhisperContext::new_with_params(
        &model_path,
        WhisperContextParameters {
            use_gpu: true,
            flash_attn: false,
            gpu_device: 0,
            dtw_parameters: DtwParameters::default(),
        },
    )?)
}

// Send audio to whisper for transcribing
pub fn transcribe(
    whisper_config: &WhisperConfig,
    ctx: &WhisperContext,
    samples: Vec<f32>,
) -> Result<Option<String>, ErrTranscribe> {
    let resampled = resample(samples, 48000, 16000)?;

    // Whisper parameters
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(whisper_config.language.as_deref());
    params.set_translate(whisper_config.translate);
    params.set_no_context(whisper_config.no_context);
    params.set_single_segment(true);
    params.set_print_realtime(false);
    params.set_print_progress(false);

    // Create whisper state
    let mut state = ctx.create_state()?;
    // Transcribe
    // TODO: Pad recordings that are too short
    state.full(params, &resampled)?;

    // Get number of output segments
    let n_segments = state.full_n_segments()?;
    // Create empty result string to fill
    let mut result = String::new();

    // Loop through segments
    for i in 0..n_segments {
        // Add each segment to the result string
        result.push_str(state.full_get_segment_text(i)?.as_str());
    }

    // Discard empty results
    if result.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(result))
    }
}
