mod audio_jack;
mod piper;
mod util;
mod whisper;

use log::info;
use serde::Deserialize;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
    },
    thread::{self},
};
use webrtc_vad::Vad;
use whisper_rs::WhisperContext;

use crate::piper::play_tts;

// Configuration struct
#[derive(Deserialize, Clone, Debug)]
struct Config {
    audio_jack: audio_jack::AudioJackConfig,
    whisper: whisper::WhisperConfig,
}

enum ProcessUnit {
    Continue(Vec<f32>),
    Quit,
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

                        // Transcribe
                        if let Some(result) =
                            whisper::transcribe(&config.whisper, &whisper_ctx, samples.clone())
                                .unwrap()
                        {
                            // Play TTS
                            play_tts(play_buffer.clone(), result).unwrap();
                        }
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
    let mut piper = piper::setup_piper().unwrap();

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

    // Start jack client
    let (temp_disconnected, active_client) =
        audio_jack::setup_jack(&config.audio_jack, audio_tx_cloned, play_buffer).unwrap();

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
            .connect_ports_by_name(&config.audio_jack.input_port, &port)
            .unwrap();
    }

    // Kill TTS
    piper.kill().unwrap();

    Ok(())
}
