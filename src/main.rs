mod util;
mod whisper;

use hound::WavReader;
use jack::*;
use log::{info, warn};
use serde::Deserialize;
use std::{
    collections::VecDeque,
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
use whisper_rs::WhisperContext;

use crate::util::resample;

// Configuration struct
#[derive(Deserialize, Clone, Debug)]
struct Config {
    audio: AudioConfig,
    whisper: whisper::WhisperConfig,
}

#[derive(Deserialize, Clone, Debug)]
struct AudioConfig {
    input_port: String,
    output_ports: Vec<String>,
}

enum ProcessUnit {
    Continue(Vec<f32>),
    Quit,
}

fn translate_and_play(
    config: &Config,
    play_buffer: Arc<Mutex<VecDeque<f32>>>,
    ctx: &WhisperContext,
    samples: Vec<f32>,
) {
    // Transcribe
    let result = whisper::transcribe(&config.whisper, ctx, samples.clone());

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
    let whisper_ctx = whisper::setup_whisper(config.whisper.clone()).unwrap();

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
