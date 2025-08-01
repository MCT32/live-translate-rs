use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, mpsc::Sender},
};

use jack::{
    AsyncClient, AudioIn, AudioOut, Client, ClientOptions, Control, ProcessScope,
    contrib::ClosureProcessHandler,
};
use log::{info, warn};
use serde::Deserialize;

use crate::ProcessUnit;

#[derive(Deserialize, Clone, Debug)]
pub struct AudioJackConfig {
    pub input_port: String,
    pub output_ports: Vec<String>,
}

pub fn setup_jack(
    config: &AudioJackConfig,
    audio_tx: Sender<ProcessUnit>,
    play_buffer: Arc<Mutex<VecDeque<f32>>>,
) -> (
    Vec<String>,
    AsyncClient<(), ClosureProcessHandler<(), impl FnMut(&Client, &ProcessScope) -> Control>>,
) {
    // Initialise jack client
    let (client, _status) =
        Client::new("rust_jack_client", ClientOptions::NO_START_SERVER).unwrap();

    // Register input port
    let in_port = client
        .register_port("input_MONO", AudioIn::default())
        .unwrap();

    // Regsiter output port
    let mut out_port = client
        .register_port("output_MONO", AudioOut::default())
        .unwrap();

    // Connect input
    client
        .connect_ports_by_name(&config.input_port, in_port.name().unwrap().as_str())
        .unwrap();

    // List of connections before program
    let mut temp_disconnected: Vec<String> = vec![];

    // Connect output
    // TODO: Probably don't need to clone here
    for port in config.output_ports.clone() {
        if let Some(port) = client.port_by_name(&port) {
            // Connect output to port
            // TODO: Error handling
            client
                .connect_ports(&out_port, &port)
                .expect("Couldnt connect ports");

            // Check for microphone connection
            if port.is_connected_to(&config.input_port).unwrap() {
                info!(
                    "Port {} connected to input, temporarily disconnecting",
                    port.name().unwrap()
                );

                // Add to list
                temp_disconnected.push(port.name().unwrap());

                // Disconnect ports
                client
                    .disconnect_ports_by_name(&config.input_port, &port.name().unwrap())
                    .unwrap();
            }
        } else {
            warn!("Port {} doesn't exist!", port);
        }
    }

    // Jack client callback
    let process = jack::contrib::ClosureProcessHandler::new(
        move |_: &Client, ps: &ProcessScope| -> jack::Control {
            // Get audio from input
            let in_buf = in_port.as_slice(ps);

            audio_tx
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

    // Start jack client
    (
        temp_disconnected,
        client.activate_async((), process).unwrap(),
    )
}
