use std::{
    collections::VecDeque,
    fmt::Display,
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
};

use log::{error, info, warn};
use serde::Deserialize;

use crate::util::resample;

#[derive(Debug)]
pub enum ErrSetupPiper {
    IoError(std::io::Error),
    CouldNotCreateEnv,
    CouldNotInstallDeps,
    CouldNotDownloadModel,
}

impl Display for ErrSetupPiper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IoError(io_error) => write!(f, "{}", io_error),
            Self::CouldNotCreateEnv => {
                write!(f, "Could not create python virtual environment for piper")
            }
            Self::CouldNotInstallDeps => write!(f, "Could not install python dependencies"),
            Self::CouldNotDownloadModel => write!(f, "Could not download piper model!"),
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

#[derive(Deserialize, Clone, Debug)]
pub struct PiperConfig {
    pub model: String,
}

// Pipe output to log and run
fn run_command_with_log(command: &mut Command) -> Result<Child, std::io::Error> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        thread::spawn(move || {
            for line in reader.lines() {
                match line {
                    Ok(line) => info!("[stdout] {}", line),
                    Err(err) => error!("Error reading stdout: {}", err),
                }
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        let reader = BufReader::new(stderr);
        thread::spawn(move || {
            for line in reader.lines() {
                match line {
                    Ok(line) => info!("[stderr] {}", line),
                    Err(err) => error!("Error reading stderr: {}", err),
                }
            }
        });
    }

    Ok(child)
}

// Make sure dependencies are installed and start piper
// TODO: Make some optional params configurable
pub fn setup_piper(config: &PiperConfig) -> Result<Child, ErrSetupPiper> {
    // Virtual environment
    const ENV_PATH: &str = "./env";

    // Create virtual environment of it doesn't already exist
    if !Path::new(ENV_PATH).exists() {
        warn!("Python virtual environment does not exist, creating now");

        let status =
            run_command_with_log(Command::new("python3").args(["-m", "venv", ENV_PATH]))?.wait()?;
        if !status.success() {
            return Err(ErrSetupPiper::CouldNotCreateEnv);
        }
    }

    // Install depencencies
    let status = run_command_with_log(Command::new(format!("{}/bin/pip", ENV_PATH)).args([
        "install",
        "--upgrade",
        "pip",
        "piper-tts",
        "flask",
    ]))?
    .wait()?;
    if !status.success() {
        return Err(ErrSetupPiper::CouldNotInstallDeps);
    }

    // Download missing model
    if !std::fs::exists(format!("./{}.onnx", config.model))? {
        warn!("Piper model not found, downloading now");

        let status =
            run_command_with_log(Command::new(format!("{}/bin/python", ENV_PATH)).args([
                "-m",
                "piper.download_voices",
                &config.model,
            ]))?
            .wait()?;
        if !status.success() {
            return Err(ErrSetupPiper::CouldNotDownloadModel);
        }
    };

    // Run server
    let piper = run_command_with_log(Command::new(format!("{}/bin/python", ENV_PATH)).args([
        "-m",
        "piper.http_server",
        "-m",
        config.model.as_str(),
    ]))?;

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
