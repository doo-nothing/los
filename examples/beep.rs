//! Diagnostic: open the default output exactly like the mixer does and
//! play 3 seconds of A440. Prints the device cpal picked.
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

fn main() -> anyhow::Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no output device"))?;
    println!("device: {:?}", device.name());
    let config = device.default_output_config()?;
    println!("config: {:?}", config);
    let rate = config.sample_rate().0 as f32;
    let channels = config.channels() as usize;
    let mut phase = 0.0_f32;
    let stream = device.build_output_stream(
        &config.into(),
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            for frame in data.chunks_mut(channels) {
                let s = (phase * std::f32::consts::TAU).sin() * 0.3;
                phase = (phase + 440.0 / rate).fract();
                for ch in frame.iter_mut() {
                    *ch = s;
                }
            }
        },
        |e| eprintln!("stream error: {e}"),
        None,
    )?;
    stream.play()?;
    println!("playing 3 s of A440 …");
    std::thread::sleep(std::time::Duration::from_secs(3));
    println!("done");
    Ok(())
}
