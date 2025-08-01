use std::{
    collections::VecDeque,
    path::Path,
    process::{Child, Command},
    sync::{Arc, Mutex},
};

use crate::util::resample;

// Make sure dependencies are installed and start piper
pub fn setup_piper() -> Child {
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

pub fn play_tts(play_buffer: Arc<Mutex<VecDeque<f32>>>, message: String) {
    // Get TTS from server
    let http_client = reqwest::blocking::Client::new();
    let voice = http_client
        .post("http://localhost:5000")
        .body(format!("{{ \"text\": \"{}\" }}", message))
        .send()
        .unwrap()
        .bytes()
        .unwrap();

    // Create reader to parse TTS outout
    let mut reader = hound::WavReader::new(std::io::Cursor::new(voice)).unwrap();
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
