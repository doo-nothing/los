//! Diagnostic: peek a ring's indices and latest slot (non-destructive).
use los::shm::AudioRingbuf;

fn main() -> anyhow::Result<()> {
    for name in ["/los_audio_swarm_0", "/los_audio_tone_0", "/los_mix_print", "/los_send_a", "/los_send_b"] {
        match AudioRingbuf::open(name) {
            Ok(rb) => {
                let mut buf = vec![0.0_f32; rb.slot_len()];
                for _ in 0..3 {
                    let w = rb.write_index();
                    let avail = rb.available();
                    let got = rb.peek_latest(&mut buf).unwrap_or(false);
                    let peak = buf.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
                    println!(
                        "{name}: w={w} avail={avail} peek={got} peak={peak:.4} ch={} fr={} slots={}",
                        rb.channels(), rb.slot_frames(), rb.num_slots()
                    );
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
            }
            Err(e) => println!("{name}: open failed: {e}"),
        }
    }
    Ok(())
}
