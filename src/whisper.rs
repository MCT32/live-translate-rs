use log::{info, warn};
use serde::Deserialize;
use whisper_rs::{
    DtwParameters, FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters,
    WhisperError,
};

use crate::util::resample;

#[derive(Deserialize, Clone, Debug)]
pub struct WhisperConfig {
    pub model: String,
    pub language: Option<String>, // TODO: See if language can be validated during parsing
    pub translate: bool,
    pub no_context: bool,
    pub single_segment: bool, // TODO: Look into hardcoding this to simplify programming
    pub print_realtime: bool, // TODO: Probably hardcode this
    pub print_progress: bool, // TODO: Probably hardcode this
}

// Load whisper
pub fn setup_whisper(config: WhisperConfig) -> Result<WhisperContext, WhisperError> {
    // Get relative path
    let model_path = format!("whisper/ggml-{}.bin", config.model);

    // Ensure whisper directory exists
    // TODO: Improve error handling
    let _ = std::fs::create_dir("whisper");

    // Check model exists
    if !std::fs::exists(&model_path).unwrap() {
        warn!("Model {} not found, attempting to download", model_path);

        // Construct url
        // TODO: Maybe make this configurable
        let url = format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{}.bin?download=true",
            config.model
        );

        // Create model file
        let mut model_file = std::fs::File::create(&model_path).unwrap();

        // Download model file
        // TODO: Add a progress bar
        let mut download = reqwest::blocking::get(url).unwrap();

        // Copy contents
        std::io::copy(&mut download, &mut model_file).unwrap();

        info!("Model {} downloaded", config.model);
    }

    // Create the context and load the model
    WhisperContext::new_with_params(
        &model_path,
        WhisperContextParameters {
            use_gpu: true,
            flash_attn: false,
            gpu_device: 0,
            dtw_parameters: DtwParameters::default(),
        },
    )
}

// Send audio to whisper for transcribing
// TODO: Error propigation
pub fn transcribe(
    whisper_config: &WhisperConfig,
    ctx: &WhisperContext,
    samples: Vec<f32>,
) -> String {
    let resampled = resample(samples, 48000, 16000).unwrap();

    // Whisper parameters
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(whisper_config.language.as_deref());
    params.set_translate(whisper_config.translate);
    params.set_no_context(whisper_config.no_context);
    params.set_single_segment(whisper_config.single_segment);
    params.set_print_realtime(whisper_config.print_realtime);
    params.set_print_progress(whisper_config.print_progress);

    // Create whisper state
    let mut state = ctx.create_state().unwrap();
    // Transcribe
    // TODO: Pad recordings that are too short
    state.full(params, &resampled).unwrap();

    // Get number of output segments
    let n_segments = state.full_n_segments().unwrap();
    // Create empty result string to fill
    let mut result = String::new();

    // Loop through segments
    for i in 0..n_segments {
        // Add each segment to the result string
        result.push_str(state.full_get_segment_text(i).unwrap().as_str());
    }

    // Return result
    result
}
