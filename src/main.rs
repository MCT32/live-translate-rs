mod audio_jack;
mod piper;
mod util;
mod whisper;

use log::{error, info};
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

// TODO: Add tests

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
                let is_voice = match vad.is_voice_segment(&samples_int) {
                    Ok(is_voice) => is_voice,
                    Err(_) => {
                        // No error returned >:(
                        // https://github.com/kaegi/webrtc-vad/issues/9
                        error!("VAD could not evaluate if the audio was voice!");
                        continue;
                    }
                };

                // If recording already started
                if recording {
                    // Add samples to recording buffer
                    samples.append(&mut in_buf.to_vec());

                    // If voice activity detected
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
                        match whisper::transcribe(&config.whisper, &whisper_ctx, samples.clone()) {
                            Ok(result) => {
                                if let Some(result) = result {
                                    // Play TTS
                                    play_tts(play_buffer.clone(), result).unwrap();
                                }
                            }
                            Err(err) => error!("Could not transcribe audio!\n{}", err),
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

fn main() {
    // Initialise logger
    // Custom format to force newlines, allowing raw mode so keys can be retrieved without pressing enter
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();

    // Load configuration file
    // TODO: Make tool for creating config if one isnt found
    // TODO: Potentially create macro for this pattern
    // TODO: Reconnect ports after disconnection when error occurs, where applicable
    // TODO: Kill piper server when error occurs, where applicable
    let config = match std::fs::read_to_string("config.toml") {
        Ok(content) => content,
        Err(_) => {
            error!("Could not read config file!");
            return;
        }
    };

    // Parse TOML
    let config: Arc<Config> = Arc::new(match toml::from_str(&config) {
        Ok(parsed) => parsed,
        Err(err) => {
            error!("Could not parse config file!\n{}", err);
            return;
        }
    });

    // Load whisper
    let whisper_ctx = match whisper::setup_whisper(config.whisper.clone()) {
        Ok(ctx) => ctx,
        Err(err) => {
            error!("Could not set up whisper!\n{}", err);
            return;
        }
    };

    // Start TTS server
    let mut piper = match piper::setup_piper() {
        Ok(child) => child,
        Err(err) => {
            error!("Could not start piper server!\n{}", err);
            return;
        }
    };

    // Channel for sending audio from jack thread to processing thread
    let (audio_tx, audio_rx) = std::sync::mpsc::channel::<ProcessUnit>();

    // Buffer for playing audio
    let play_buffer: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));

    // Clone arcs for processing thread
    let play_buffer_cloned = play_buffer.clone();
    let config_cloned = config.clone();

    // Spawn processing thread
    let audio_thread = match thread::Builder::new()
        .name("audio_processor".to_owned())
        .spawn(move || process_audio(whisper_ctx, config_cloned, play_buffer_cloned, audio_rx))
    {
        Ok(thread) => thread,
        Err(err) => {
            error!("Could not start audio processing thread!\n{}", err);
            return;
        }
    };

    // Clone for use in closure
    let audio_tx_cloned = audio_tx.clone();

    // Start jack client
    let (temp_disconnected, active_client) =
        match audio_jack::setup_jack(&config.audio_jack, audio_tx_cloned, play_buffer) {
            Ok(client) => client,
            Err(err) => {
                error!("Could not set up jack client!\n{}", err);
                return;
            }
        };

    // Bool so that program can safely exit
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    // Handler for exit
    if let Err(err) = ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    }) {
        error!("Could not create crtlc handle!\n{}", err);
        return;
    };

    // Keep running until exit
    while running.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    // Stop processing thread
    if let Err(err) = audio_tx.send(ProcessUnit::Quit) {
        error!(
            "Could not send stop signal to audio processing thread!\n{}",
            err
        );
    };
    if let Err(_) = audio_thread.join() {
        error!("Could not join audio processing thread!");
    };

    // Stop jack client
    let (client, _, _) = match active_client.deactivate() {
        Ok(client) => client,
        Err(err) => {
            error!("Could not deactivate jack client!\n{}", err);
            return;
        }
    };

    // Reconnect disconnected ports
    for port in temp_disconnected {
        if let Err(err) = client.connect_ports_by_name(&config.audio_jack.input_port, &port) {
            error!(
                "Could not reconnect port {} to {}!\n{}",
                &config.audio_jack.input_port, &port, err
            );
        }
    }

    // Kill TTS
    if let Err(err) = piper.kill() {
        error!("Could not kill piper server!\n{}", err);
    };
}
