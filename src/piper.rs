use std::{
    collections::VecDeque,
    fmt::Display,
    path::Path,
    process::{Child, Command},
    sync::{Arc, Mutex},
};

use log::warn;

use crate::util::resample;

#[derive(Debug)]
pub enum ErrSetupPiper {
    IoError(std::io::Error),
    CouldNotCreateEnv,
    CouldNotInstallDeps,
}

impl Display for ErrSetupPiper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IoError(io_error) => write!(f, "{}", io_error),
            Self::CouldNotCreateEnv => {
                write!(f, "Could not create python virtual environment for piper")
            }
            Self::CouldNotInstallDeps => write!(f, "Could not install python dependencies"),
        }
    }
}

impl std::error::Error for ErrSetupPiper {}

impl From<std::io::Error> for ErrSetupPiper {
    fn from(value: std::io::Error) -> Self {
        Self::IoError(value)
    }
}

#[derive(Debug)]
pub enum ErrPlayTTS {
    ReqwestError(reqwest::Error),
    HoundError(hound::Error),
    ResampleError(speexdsp_resampler::Error),
}

impl Display for ErrPlayTTS {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReqwestError(error) => write!(f, "{}", error),
            Self::HoundError(error) => write!(f, "{}", error),
            Self::ResampleError(error) => write!(f, "{:?}", error),
        }
    }
}

impl std::error::Error for ErrPlayTTS {}

impl From<reqwest::Error> for ErrPlayTTS {
    fn from(value: reqwest::Error) -> Self {
        Self::ReqwestError(value)
    }
}

impl From<hound::Error> for ErrPlayTTS {
    fn from(value: hound::Error) -> Self {
        Self::HoundError(value)
    }
}

impl From<speexdsp_resampler::Error> for ErrPlayTTS {
    fn from(value: speexdsp_resampler::Error) -> Self {
        Self::ResampleError(value)
    }
}

// Make sure dependencies are installed and start piper
pub fn setup_piper() -> Result<Child, ErrSetupPiper> {
    // Virtual environment
    const ENV_PATH: &str = "./env";

    // Name of TTS model
    // TODO: Make configurable
    // TODO: Handle model downloading
    let model = "en_US-lessac-high";

    // Create virtual environment of it doesn't already exist
    // TODO: Try printing python stdout/stderr through log
    if !Path::new(ENV_PATH).exists() {
        warn!("Python virtual environment does not exist, creating now");

        let status = Command::new("python3")
            .args(["-m", "venv", ENV_PATH])
            .status()?;
        if !status.success() {
            return Err(ErrSetupPiper::CouldNotCreateEnv);
        }
    }

    // Install depencencies
    let status = Command::new(format!("{}/bin/pip", ENV_PATH))
        .args(["install", "--upgrade", "pip", "piper-tts", "flask"])
        .status()?;
    assert!(status.success(), "Couldn't install python dependencies");
    if !status.success() {
        return Err(ErrSetupPiper::CouldNotInstallDeps);
    }

    // Run server
    let piper = Command::new(format!("{}/bin/python", ENV_PATH))
        .args(["-m", "piper.http_server", "-m", model])
        .spawn()?;

    Ok(piper)
}

pub fn play_tts(play_buffer: Arc<Mutex<VecDeque<f32>>>, message: String) -> Result<(), ErrPlayTTS> {
    // Get TTS from server
    let http_client = reqwest::blocking::Client::new();
    let voice = http_client
        .post("http://localhost:5000")
        .body(format!("{{ \"text\": \"{}\" }}", message))
        .send()?
        .bytes()?;

    // Create reader to parse TTS outout
    let mut reader = hound::WavReader::new(std::io::Cursor::new(voice))?;
    // Create buffer for TTS samples
    let mut samples: Vec<f32> = vec![];

    // Loop through samples
    // TODO: Handle different sample formats instead of hardcoding i16
    for sample in reader.samples::<i16>() {
        // Convert sample to floats and scale accordingly
        samples.push(sample? as f32 / i16::MAX as f32);
    }

    // Get sample rate
    let samplerate = reader.spec().sample_rate as usize;

    let resampled = resample(samples, samplerate, 48000)?;

    // Lock play buffer
    let mut play_buffer = play_buffer.lock().unwrap();
    // Add resulting TTS audio to the play buffer
    play_buffer.append(&mut Into::<VecDeque<_>>::into(resampled));

    Ok(())
}
