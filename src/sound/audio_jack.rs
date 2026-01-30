use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, mpsc::Sender},
};

use jack::{
    AsyncClient, AudioIn, AudioOut, Client, ClientOptions, Control, Port, ProcessScope,
    contrib::ClosureProcessHandler,
};
use log::{error, info, warn};
use serde::Deserialize;

use crate::{ProcessUnit, sound::AudioClient};

#[derive(Deserialize, Clone, Debug)]
pub struct JackConfig {
    pub input_port: String,
    pub output_ports: Vec<String>,
}

pub struct JackClient {
    client: Option<Client>,
    async_client: Option<
        AsyncClient<
            (),
            ClosureProcessHandler<(), Box<dyn FnMut(&Client, &ProcessScope) -> Control + Send>>,
        >,
    >,
    temp_disconnected: Vec<String>,
    input_name: String,
    in_port: Option<Port<AudioIn>>,
    out_port: Option<Port<AudioOut>>,
}

impl AudioClient for JackClient {
    type Config = JackConfig;
    type Error = jack::Error;

    fn new(config: &Self::Config) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        // Initialise jack client
        let (client, _status) = Client::new("rust_jack_client", ClientOptions::NO_START_SERVER)?;

        // Register input port
        let in_port = client.register_port("input_MONO", AudioIn::default())?;

        // Regsiter output port
        let out_port = client.register_port("output_MONO", AudioOut::default())?;

        // Connect input
        let input_name = config.input_port.clone();
        client.connect_ports_by_name(&input_name, in_port.name()?.as_str())?;

        // List of connections before program
        let mut temp_disconnected: Vec<String> = vec![];

        // Connect output
        for port in config.output_ports.clone() {
            if let Some(port) = client.port_by_name(&port) {
                // Connect output to port
                client.connect_ports(&out_port, &port)?;

                // Check for microphone connection
                if port.is_connected_to(&config.input_port)? {
                    info!(
                        "Port {} connected to input, temporarily disconnecting",
                        port.name()?
                    );

                    // Add to list
                    temp_disconnected.push(port.name()?);

                    // Disconnect ports
                    client.disconnect_ports_by_name(&config.input_port, &port.name()?)?;
                }
            } else {
                warn!("Port {} doesn't exist!", port);
            }
        }

        Ok(Self {
            client: Some(client),
            temp_disconnected,
            input_name,
            in_port: Some(in_port),
            out_port: Some(out_port),
            async_client: None,
        })
    }

    fn start(
        &mut self,
        audio_tx: Sender<ProcessUnit>,
        play_buffer: Arc<Mutex<VecDeque<f32>>>,
    ) -> Result<(), Self::Error> {
        let in_port = self.in_port.take().unwrap();
        let mut out_port = self.out_port.take().unwrap();

        let handler: Box<dyn FnMut(&Client, &ProcessScope) -> Control + Send> =
            Box::new(move |_: &Client, ps: &ProcessScope| -> Control {
                // Get audio from input
                let in_buf = in_port.as_slice(ps);

                if let Err(err) = audio_tx.send(ProcessUnit::Continue(in_buf.to_vec())) {
                    error!("Could not send audio for processing!\n{}", err);
                    return jack::Control::Continue;
                };

                // Create buffer to write sound output
                let out_buf = out_port.as_mut_slice(ps);

                {
                    // Lock the play buffer
                    let mut play_buffer = match play_buffer.lock() {
                        Ok(buffer) => buffer,
                        Err(err) => {
                            error!("Could not lock play buffer!\n{}", err);
                            return jack::Control::Continue;
                        }
                    };

                    // Iterate through samples
                    for frame in out_buf.iter_mut() {
                        // Pop sample from buffer if its available, otherwise output silence
                        *frame = play_buffer.pop_front().unwrap_or(0.0);
                    }
                }

                // Tell jack to continue
                jack::Control::Continue
            });

        // Jack client callback
        let process = ClosureProcessHandler::new(handler);

        let client = self.client.take().unwrap();

        // Start jack client
        self.async_client = Some(client.activate_async((), process)?);

        Ok(())
    }

    fn stop(&mut self) {
        // Stop jack client
        let (client, _, _) = match self.async_client.take().unwrap().deactivate() {
            Ok(client) => client,
            Err(err) => {
                error!("Could not deactivate jack client!\n{}", err);
                return;
            }
        };

        // Reconnect disconnected ports
        for port in &self.temp_disconnected {
            if let Err(err) = client.connect_ports_by_name(&self.input_name, &port) {
                error!(
                    "Could not reconnect port {} to {}!\n{}",
                    &self.input_name, &port, err
                );
            }
        }
    }
}
