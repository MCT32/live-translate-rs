use std::{collections::VecDeque, io::Cursor, sync::{Arc, Mutex}, thread};
use hound::WavReader;
use jack::*;
use log::{info, warn};
use serde::Deserialize;
use whisper_rs::{DtwParameters, FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

// Configuration struct
#[derive(Deserialize, Debug)]
struct Config {
    audio: AudioConfig,
    whisper: WhisperConfig,
}

#[derive(Deserialize, Debug)]
struct AudioConfig {
    input_port: String,
    output_ports: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct WhisperConfig {
    language: Option<String>,   // TODO: See if language can be validated during parsing
    translate: bool,
    no_context: bool,
    single_segment: bool,       // TODO: Look into hardcoding this to simplify programming
    print_realtime: bool,       // TODO: Probably hardcode this
    print_progress: bool,       // TODO: Probably hardcode this
}

// Calculate RMS from samples
fn rms(buf: &[f32]) -> f32 {
    ((1.0 / buf.len() as f32) * buf.iter().map(|x| x.powi(2)).sum::<f32>()).sqrt()
}

fn resample(samples: Vec<f32>, from: usize, to: usize) -> Result<Vec<f32>, speexdsp_resampler::Error> {
    // Create resampler
    // TODO: Figure out putpose of quality param
    let mut resampler = speexdsp_resampler::State::new(1, from, to, 4)?;

    // Output buffer
    // TODO: See if filling the buffer in necessary
    // TODO: Find out what the + 512 is for
    let mut resampled = vec![0.0; ((samples.len() as f64 * to as f64 / from as f64).ceil() as usize) + 512];

    // Downsample
    // TODO: Figure out what index is for
    resampler.process_float(0, &samples, &mut resampled)?;

    Ok(resampled)
}

// Send audio to whisper for transcribing
fn transcribe(config: Arc<Config>, ctx: Arc<Mutex<WhisperContext>>, samples: Vec<f32>) -> String {
    // Lock whisper context
    let ctx = ctx.lock().unwrap();

    let resampled = resample(samples, 48000, 16000).unwrap();

    // Whisper parameters
    // TODO: Make configurable
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

fn translate_and_play(config: Arc<Config>, play_buffer: Arc<Mutex<VecDeque<f32>>>, ctx: Arc<Mutex<WhisperContext>>, samples: Vec<f32>) {
    // Transcribe
    let result = transcribe(config, ctx, samples.clone());

    // Discard empty results
    if result.trim().is_empty() {
        return
    }

    // Get TTS from server
    // TODO: Check server is up before running
    // TODO: Make address and other parameters configurable
    // TODO: Start server on program start
    let http_client = reqwest::blocking::Client::new();
    let voice = http_client.post("http://localhost:5000")
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialise logger
    env_logger::Builder::new().filter_level(log::LevelFilter::Info).init();

    // Load configuration file
    // TODO: Make tool for creating config if one isnt found
    let config = std::fs::read_to_string("config.toml").expect("Unable to read config file!");

    // Parse TOML
    let config: Arc<Config> = Arc::new(toml::from_str(&config).expect("Couldn't parse config file!"));

    // Tell whisper to use log
    whisper_rs::install_logging_hooks();

    // Load whisper
    // TODO: Handle downloading models
    // TODO: Make model configurable
    let whisper_ctx = Arc::new(Mutex::new(WhisperContext::new_with_params("whisper/ggml-large-v2.bin", WhisperContextParameters {
        use_gpu: true,
        flash_attn: false,
        gpu_device: 0,
        dtw_parameters: DtwParameters::default(),
    }).unwrap()));

    // Initialise jack client
    let (client, _status) =
        Client::new("rust_jack_client", ClientOptions::NO_START_SERVER).unwrap();

    // Regsiter output port
    let mut out_port = client.register_port("output_MONO", AudioOut::default()).unwrap();
    
    // Connect output
    // TODO: Probably don't need to clone here
    for port in config.audio.output_ports.clone() {
        match client.connect_ports_by_name(out_port.name().unwrap().as_str(), &port) {
            Ok(_) => info!("Connected ouput to port {}", port),
            Err(err) => match err {
                jack::Error::PortAlreadyConnected(_, _) => warn!("Tried connecting output to port {}, but it was already connected", port),
                jack::Error::PortConnectionError { source: _, destination: _, code_or_message } => warn!("Couldn't connect output to port {}, {}", port, code_or_message),
                _ => return Result::Err(Box::new(err)),
            },
        }
    }

    // Register input port
    let in_port = client.register_port("input_MONO", AudioIn::default()).unwrap();
    // Connect input
    client.connect_ports_by_name(&config.audio.input_port, in_port.name().unwrap().as_str()).unwrap();

    // Recording state
    // TODO: Consider making a struct
    let mut recording: bool = false;    // Current recording status
    let mut silence: u32 = 0;           // How many blocks have been silent, used to decide when to stop recording
    let mut samples: Vec<f32> = vec![];

    // Buffer for playing audio
    // TODO: Explore the performance of this
    let play_buffer: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));

    // Jack client callback
    let process = jack::contrib::ClosureProcessHandler::new(
        move |_: &Client, ps: &ProcessScope| -> jack::Control {
            // Get audio from input
            let in_buf = in_port.as_slice(ps);

            // If recording already started
            if recording {
                // Add samples to recording buffer
                samples.append(&mut in_buf.to_vec());

                // If voice activity detected
                // TODO: Record a baseline noise level for people without noise canceling
                if rms(in_buf) > 0.0 {
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
                    let play_buffer_cloned = play_buffer.clone();
                    let whisper_ctx_cloned = whisper_ctx.clone();
                    let samples_cloned = samples.clone();
                    let config_cloned = config.clone();

                    // Spawn a new thread to handle the rest, otherwise jack hangs and user has no audio until completed
                    thread::spawn(|| {
                        // Transcbribe, translate and play result
                        translate_and_play(config_cloned, play_buffer_cloned, whisper_ctx_cloned, samples_cloned);
                    });
                }
            } else {
                // If noise level increases
                if rms(in_buf) > 0.0 {
                    // Start recording
                    info!("Recording started...");
                    recording = true;
                    samples.clear();    // Clear previous recording
                    samples.append(&mut in_buf.to_vec());
                }
            }

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

    // Loop forever
    // TODO: Make a more elegant solution
    loop {}

    // Stop jack client
    // Unreachable with current solution
    active_client.deactivate().unwrap();

    Ok(())
}
