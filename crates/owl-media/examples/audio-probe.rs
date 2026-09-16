//! Which audio devices does cpal actually see, and which of them will
//! accept a stream? `cargo run -p owl-media --example audio-probe`.

use cpal::traits::{DeviceTrait, HostTrait};

fn main() {
    let host = cpal::default_host();
    println!("host: {:?}", host.id());

    match host.default_output_device() {
        Some(d) => println!("default output: {:?}", d.description().map(|d| d.name().to_string())),
        None => println!("default output: NONE"),
    }

    println!("\nenumerating output devices:");
    let devices = match host.output_devices() {
        Ok(d) => d,
        Err(e) => {
            println!("  cannot enumerate: {e}");
            return;
        }
    };

    for device in devices {
        let name = device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "<unnamed>".into());
        match device.default_output_config() {
            Ok(config) => {
                print!(
                    "  {name:<28} {} Hz · {} ch · {:?}  ",
                    config.sample_rate(),
                    config.channels(),
                    config.sample_format()
                );
                // Configuration is not the same as being able to open it:
                // dmix will describe itself happily and then fail to start.
                let stream_config: cpal::StreamConfig = config.clone().into();
                match device.build_output_stream(
                    stream_config,
                    move |out: &mut [f32], _: &cpal::OutputCallbackInfo| out.fill(0.0),
                    |e| eprintln!("stream error: {e}"),
                    None,
                ) {
                    Ok(_) => println!("OPENS"),
                    Err(e) => println!("FAILS: {e}"),
                }
            }
            Err(e) => println!("  {name:<28} no config: {e}"),
        }
    }
}
