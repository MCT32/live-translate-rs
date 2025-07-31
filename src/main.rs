use hound::WavReader;
use jack::*;
use log::{info, warn};
use serde::Deserialize;
use std::{
    collections::VecDeque,
    fs::File,
    io::Cursor,
    path::Path,
    process::{Child, Command},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
    },
    thread::{self},
};
use webrtc_vad::Vad;
use whisper_rs::{
    DtwParameters, FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters,
    WhisperError,
};

// Configuration struct
#[derive(Deserialize, Clone, Debug)]
struct Config {
    audio: AudioConfig,
    whisper: WhisperConfig,
}

#[derive(Deserialize, Clone, Debug)]
struct AudioConfig {
    input_port: String,
    output_ports: Vec<String>,
}

#[derive(Deserialize, Clone, Debug)]
struct WhisperConfig {
    model: String,
    language: Option<String>, // TODO: See if language can be validated during parsing
    translate: bool,
    no_context: bool,
    single_segment: bool, // TODO: Look into hardcoding this to simplify programming
    print_realtime: bool, // TODO: Probably hardcode this
    print_progress: bool, // TODO: Probably hardcode this
}

enum ProcessUnit {
    Continue(Vec<f32>),
    Quit,
}

fn resample(
    samples: Vec<f32>,
    from: usize,
    to: usize,
) -> Result<Vec<f32>, speexdsp_resampler::Error> {
    // Create resampler
    // TODO: Figure out putpose of quality param
    let mut resampler = speexdsp_resampler::State::new(1, from, to, 4)?;

    // Output buffer
    // TODO: See if filling the buffer in necessary
    // TODO: Find out what the + 512 is for
    let mut resampled =
        vec![0.0; ((samples.len() as f64 * to as f64 / from as f64).ceil() as usize) + 512];

    // Downsample
    // TODO: Figure out what index is for
    resampler.process_float(0, &samples, &mut resampled)?;

    Ok(resampled)
}

// Send audio to whisper for transcribing
fn transcribe(config: &Config, ctx: &WhisperContext, samples: Vec<f32>) -> String {
    let resampled = resample(samples, 48000, 16000).unwrap();

    // Whisper parameters
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(config.whisper.language.as_deref());
    params.set_translate(config.whisper.translate);
    params.set_no_context(config.whisper.no_context);
    params.set_single_segment(config.whisper.single_segment);
    params.set_print_realtime(config.whisper.print_realtime);
    params.set_print_progress(config.whisper.print_progress);

    // Create whisper state
    let mut state = ctx.create_state().unwrap();
    // Transcribe
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

fn translate_and_play(
    config: &Config,
    play_buffer: Arc<Mutex<VecDeque<f32>>>,
    ctx: &WhisperContext,
    samples: Vec<f32>,
) {
    // Transcribe
    let result = transcribe(config, ctx, samples.clone());

    // Discard empty results
    if result.trim().is_empty() {
        return;
    }

    // Get TTS from server
    // TODO: Check server is up before running
    // TODO: Make address and other parameters configurable
    // TODO: Start server on program start
    let http_client = reqwest::blocking::Client::new();
    let voice = http_client
        .post("http://localhost:5000")
        .body(format!("{{ \"text\": \"{}\" }}", result))
        .send()
        .unwrap()
        .bytes()
        .unwrap();

    // Create reader to parse TTS outout
    let mut reader = WavReader::new(Cursor::new(voice)).unwrap();
    // Create buffer for TTS samples
    let mut samples: Vec<f32> = vec![];

    // Loop through samples
    // TODO: Handle different sample formats instead of hardcoding i16
    for sample in reader.samples::<i16>() {
        // Convert sample to floats and scale accordingly
        samples.push(sample.unwrap() as f32 / i16::MAX as f32);
    }

    // Get sample rate
    let samplerate = reader.spec().sample_rate as usize;

    let resampled = resample(samples, samplerate, 48000).unwrap();

    // Lock play buffer
    let mut play_buffer = play_buffer.lock().unwrap();
    // Add resulting TTS audio to the play buffer
    play_buffer.append(&mut Into::<VecDeque<_>>::into(resampled));
}

// Load whisper
fn setup_whisper(config: WhisperConfig) -> Result<WhisperContext, WhisperError> {
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
        let mut model_file = File::create(&model_path).unwrap();

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

// Make sure dependencies are installed and start piper
fn setup_piper() -> Child {
    // Virtual environment
    const ENV_PATH: &str = "./env";

    // Name of TTS model
    // TODO: Make configurable
    // TODO: Handle model downloading
    let model = "en_US-lessac-high";

    // Create virtual environment of it doesn't already exist
    if !Path::new(ENV_PATH).exists() {
        let status = Command::new("python3")
            .args(["-m", "venv", ENV_PATH])
            .status()
            .unwrap();
        assert!(status.success(), "Couldn't create virtual environment");
    }

    // Install depencencies
    let status = Command::new(format!("{}/bin/pip", ENV_PATH))
        .args(["install", "--upgrade", "pip", "piper-tts", "flask"])
        .status()
        .unwrap();
    assert!(status.success(), "Couldn't install python dependencies");

    // Run server
    let piper = Command::new(format!("{}/bin/python", ENV_PATH))
        .args(["-m", "piper.http_server", "-m", model])
        .spawn()
        .unwrap();

    piper
}

fn process_audio(
    whisper_ctx: WhisperContext,
    config: Arc<Config>,
    play_buffer: Arc<Mutex<VecDeque<f32>>>,
    audio: Receiver<ProcessUnit>,
) {
    // Recording state
    let mut recording: bool = false; // Current recording status
    let mut silence: u32 = 0; // How many blocks have been silent, used to decide when to stop recording
    let mut samples: Vec<f32> = vec![];

    // Voice activity detector instance
    let mut vad = Vad::new_with_rate(webrtc_vad::SampleRate::Rate48kHz);

    for unit in audio {
        match unit {
            ProcessUnit::Continue(in_buf) => {
                // Convert to i16 for VAD
                let mut samples_int = in_buf
                    .iter()
                    .map(|x| (x.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16)
                    .collect::<Vec<_>>();

                // Truncate to correct size
                samples_int.truncate(960);

                // Detect voice activity
                let is_voice = vad.is_voice_segment(&samples_int).unwrap();

                // If recording already started
                if recording {
                    // Add samples to recording buffer
                    samples.append(&mut in_buf.to_vec());

                    // If voice activity detected
                    // TODO: Record a baseline noise level for people without noise canceling
                    if is_voice {
                        // Reset silence counter
                        silence = 0;
                    } else {
                        // Increment silence counter
                        silence += 1;
                    }

                    // If there has been enough silence
                    // TODO: Make duration configurable
                    if silence >= 10 {
                        // Finish recording
                        info!("Recording finished");
                        recording = false;

                        // Clone Arcs for use in closure
                        let samples_cloned = samples.clone();

                        // Transcbribe, translate and play result
                        translate_and_play(
                            &config,
                            play_buffer.clone(),
                            &whisper_ctx,
                            samples_cloned,
                        );
                    }
                } else {
                    // If noise level increases
                    if is_voice {
                        // Start recording
                        info!("Recording started...");
                        recording = true;
                        samples.clear(); // Clear previous recording
                        samples.append(&mut in_buf.to_vec());
                    }
                }
            }
            ProcessUnit::Quit => break,
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialise logger
    // Custom format to force newlines, allowing raw mode so keys can be retrieved without pressing enter
    // TODO: Find another solution to this without replacing the format
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();

    // Load configuration file
    // TODO: Make tool for creating config if one isnt found
    let config = std::fs::read_to_string("config.toml").expect("Unable to read config file!");

    // Parse TOML
    let config: Arc<Config> =
        Arc::new(toml::from_str(&config).expect("Couldn't parse config file!"));

    // Tell whisper to use log
    whisper_rs::install_logging_hooks();

    // Load whisper
    let whisper_ctx = setup_whisper(config.whisper.clone()).unwrap();

    // Start TTS server
    let mut piper = setup_piper();

    // Initialise jack client
    let (client, _status) =
        Client::new("rust_jack_client", ClientOptions::NO_START_SERVER).unwrap();

    // Register input port
    let in_port = client
        .register_port("input_MONO", AudioIn::default())
        .unwrap();
    // Connect input
    client
        .connect_ports_by_name(&config.audio.input_port, in_port.name().unwrap().as_str())
        .unwrap();

    // Regsiter output port
    let mut out_port = client
        .register_port("output_MONO", AudioOut::default())
        .unwrap();

    // List of connections before program
    let mut temp_disconnected: Vec<String> = vec![];

    // Connect output
    // TODO: Probably don't need to clone here
    for port in config.audio.output_ports.clone() {
        if let Some(port) = client.port_by_name(&port) {
            // Connect output to port
            // TODO: Error handling
            client
                .connect_ports(&out_port, &port)
                .expect("Couldnt connect ports");

            // Check for microphone connection
            if port.is_connected_to(&config.audio.input_port).unwrap() {
                info!(
                    "Port {} connected to input, temporarily disconnecting",
                    port.name().unwrap()
                );

                // Add to list
                temp_disconnected.push(port.name().unwrap());

                // Disconnect ports
                client
                    .disconnect_ports_by_name(&config.audio.input_port, &port.name().unwrap())
                    .unwrap();
            }
        } else {
            warn!("Port {} doesn't exist!", port);
        }
    }

    // Channel for sending audio from jack thread to processing thread
    let (audio_tx, audio_rx) = std::sync::mpsc::channel::<ProcessUnit>();

    // Buffer for playing audio
    // TODO: Explore the performance of this
    let play_buffer: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));

    // Clone arcs for processing thread
    let play_buffer_cloned = play_buffer.clone();
    let config_cloned = config.clone();

    // Spawn processing thread
    // TODO: Name threads
    let audio_thread = thread::spawn(move || {
        process_audio(whisper_ctx, config_cloned, play_buffer_cloned, audio_rx)
    });

    // Clone for use in closure
    let audio_tx_cloned = audio_tx.clone();

    // Jack client callback
    let process = jack::contrib::ClosureProcessHandler::new(
        move |_: &Client, ps: &ProcessScope| -> jack::Control {
            // Get audio from input
            let in_buf = in_port.as_slice(ps);

            audio_tx_cloned
                .send(ProcessUnit::Continue(in_buf.to_vec()))
                .unwrap();

            // Create buffer to write sound output
            let out_buf = out_port.as_mut_slice(ps);

            {
                // Lock the play buffer
                let mut play_buffer = play_buffer.lock().unwrap();

                // Iterate through samples
                // TODO: Try without iteration
                for frame in out_buf.iter_mut() {
                    // Pop sample from buffer if its available, otherwise output silence
                    *frame = play_buffer.pop_front().unwrap_or(0.0);
                }
            }

            // Tell jack to continue
            jack::Control::Continue
        },
    );

    // Start the client
    let active_client = client.activate_async((), process).unwrap();

    // Bool so that program can safely exit
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    // Handler for exit
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl+C handler");

    // Keep running until exit
    while running.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    // Stop processing thread
    audio_tx.send(ProcessUnit::Quit).unwrap();
    audio_thread.join().unwrap();

    // Stop jack client
    let (client, _, _) = active_client.deactivate().unwrap();

    // Reconnect disconnected ports
    for port in temp_disconnected {
        client
            .connect_ports_by_name(&config.audio.input_port, &port)
            .unwrap();
    }

    // Kill TTS
    piper.kill().unwrap();

    Ok(())
}
