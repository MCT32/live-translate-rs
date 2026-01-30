use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, mpsc::Sender},
};

use serde::Deserialize;

use crate::{ProcessUnit, sound::audio_jack::JackConfig};

pub mod audio_jack;

#[derive(Deserialize, Clone, Debug)]
pub enum AudioClientType {
    Jack,
}

#[derive(Deserialize, Clone, Debug)]
pub struct AudioConfig {
    pub jack: Option<JackConfig>,
}

pub trait AudioClient: Send {
    type Config: for<'de> Deserialize<'de>;
    type Error: std::error::Error + Send + 'static;

    // Setup the client
    fn new(config: &Self::Config) -> Result<Self, Self::Error>
    where
        Self: Sized;

    // Start processing audio
    fn start(
        &mut self,
        audio_tx: Sender<ProcessUnit>,
        play_buffer: Arc<Mutex<VecDeque<f32>>>,
    ) -> Result<(), Self::Error>;

    // Stop the client
    fn stop(&mut self);
}
