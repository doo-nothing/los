//! # Standard MIDI File (SMF) parsing
//!
//! A small, self-contained reader for type-0 and type-1 `.mid` files —
//! enough to load a MIDI file into the sequencer: note on/off events
//! (with their channel and absolute tick), the division (ticks per
//! quarter note), and the initial tempo. No external dependency.

/// A single note extracted from a MIDI file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MidiNote {
    /// The MIDI channel (0–15) the note played on.
    pub channel: u8,
    /// Note-on time, in ticks from the start of the song.
    pub start_tick: u64,
    /// Note length in ticks (0 if no matching note-off was found).
    pub duration_ticks: u64,
    pub note: u8,
    pub velocity: u8,
}

/// The parsed contents of a MIDI file.
#[derive(Debug, Clone)]
pub struct MidiFile {
    /// Ticks per quarter note (the SMF division, when positive).
    pub ticks_per_quarter: u32,
    /// Tempo in microseconds per quarter note (default 500000 = 120 BPM).
    pub tempo_us: u32,
    /// All notes across all tracks, sorted by start tick.
    pub notes: Vec<MidiNote>,
}

impl MidiFile {
    /// Beats per minute from the parsed tempo.
    #[must_use]
    pub fn bpm(&self) -> f64 {
        60_000_000.0 / self.tempo_us.max(1) as f64
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }
    fn u8(&mut self) -> Result<u8, String> {
        let b = *self.data.get(self.pos).ok_or("unexpected end of file")?;
        self.pos += 1;
        Ok(b)
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(((self.u8()? as u16) << 8) | self.u8()? as u16)
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(((self.u16()? as u32) << 16) | self.u16()? as u32)
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.remaining() < n {
            return Err("unexpected end of file".into());
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    /// MIDI variable-length quantity.
    fn vlq(&mut self) -> Result<u32, String> {
        let mut value = 0u32;
        for _ in 0..4 {
            let b = self.u8()?;
            value = (value << 7) | (b & 0x7f) as u32;
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("variable-length quantity too long".into())
    }
}

/// Parse a Standard MIDI File from bytes.
pub fn parse(data: &[u8]) -> Result<MidiFile, String> {
    let mut r = Reader::new(data);
    if r.bytes(4)? != b"MThd" {
        return Err("not a MIDI file (missing MThd header)".into());
    }
    let header_len = r.u32()?;
    let _format = r.u16()?;
    let num_tracks = r.u16()?;
    let division = r.u16()?;
    // skip any extra header bytes
    if header_len > 6 {
        r.bytes((header_len - 6) as usize)?;
    }
    let ticks_per_quarter = if division & 0x8000 == 0 {
        u32::from(division)
    } else {
        // SMPTE timing: frames * subframes; approximate as ticks/quarter
        let frames = (256 - (division >> 8) as u32) & 0xff;
        let subframes = (division & 0xff) as u32;
        (frames * subframes).max(96)
    };

    let mut tempo_us = 500_000u32;
    let mut notes: Vec<MidiNote> = Vec::new();
    // pending note-ons keyed by (channel, note) → (start_tick, velocity)
    let mut pending: std::collections::HashMap<(u8, u8), (u64, u8)> =
        std::collections::HashMap::new();

    for _ in 0..num_tracks {
        if r.remaining() < 8 {
            break;
        }
        if r.bytes(4)? != b"MTrk" {
            return Err("malformed track (missing MTrk)".into());
        }
        let track_len = r.u32()? as usize;
        let track_end = r.pos + track_len;
        let mut tick = 0u64;
        let mut running_status = 0u8;
        pending.clear();
        while r.pos < track_end {
            let delta = r.vlq()?;
            tick += u64::from(delta);
            let mut status = r.u8()?;
            if status < 0x80 {
                // running status: reuse the previous status byte
                r.pos -= 1;
                status = running_status;
            } else {
                running_status = status;
            }
            match status & 0xf0 {
                0x90 => {
                    // note on (velocity 0 = note off)
                    let channel = status & 0x0f;
                    let note = r.u8()?;
                    let vel = r.u8()?;
                    if vel > 0 {
                        pending.insert((channel, note), (tick, vel));
                    } else if let Some((start, v)) = pending.remove(&(channel, note)) {
                        notes.push(MidiNote {
                            channel,
                            start_tick: start,
                            duration_ticks: tick - start,
                            note,
                            velocity: v,
                        });
                    }
                }
                0x80 => {
                    let channel = status & 0x0f;
                    let note = r.u8()?;
                    let _vel = r.u8()?;
                    if let Some((start, v)) = pending.remove(&(channel, note)) {
                        notes.push(MidiNote {
                            channel,
                            start_tick: start,
                            duration_ticks: tick - start,
                            note,
                            velocity: v,
                        });
                    }
                }
                0xa0 | 0xb0 | 0xe0 => {
                    // poly aftertouch / controller / pitch bend: 2 data bytes
                    r.bytes(2)?;
                }
                0xc0 | 0xd0 => {
                    // program change / channel aftertouch: 1 data byte
                    r.bytes(1)?;
                }
                0xf0 => {
                    match status {
                        0xff => {
                            // meta event
                            let meta = r.u8()?;
                            let len = r.vlq()? as usize;
                            let payload = r.bytes(len)?;
                            if meta == 0x51 && payload.len() == 3 {
                                tempo_us = ((payload[0] as u32) << 16)
                                    | ((payload[1] as u32) << 8)
                                    | payload[2] as u32;
                            }
                            // meta 0x2f = end of track; the loop bound handles it
                        }
                        0xf0 | 0xf7 => {
                            // sysex: skip the declared length
                            let len = r.vlq()? as usize;
                            r.bytes(len)?;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        // close any notes left hanging at the track end
        for ((channel, note), (start, v)) in pending.drain() {
            notes.push(MidiNote {
                channel,
                start_tick: start,
                duration_ticks: tick.saturating_sub(start),
                note,
                velocity: v,
            });
        }
        r.pos = track_end;
    }

    notes.sort_by_key(|n| (n.start_tick, n.note));
    Ok(MidiFile {
        ticks_per_quarter: ticks_per_quarter.max(1),
        tempo_us,
        notes,
    })
}

/// Read and parse a MIDI file from disk.
pub fn load(path: &str) -> Result<MidiFile, String> {
    let data = std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))?;
    parse(&data)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal type-0 SMF in memory: division 96, one note C4
    /// at tick 0 for 96 ticks, then E4 at tick 96.
    fn tiny_smf() -> Vec<u8> {
        let mut d = Vec::new();
        // header
        d.extend_from_slice(b"MThd");
        d.extend_from_slice(&6u32.to_be_bytes());
        d.extend_from_slice(&0u16.to_be_bytes()); // format 0
        d.extend_from_slice(&1u16.to_be_bytes()); // 1 track
        d.extend_from_slice(&96u16.to_be_bytes()); // division
                                                   // track
        let mut trk = Vec::new();
        // set tempo 500000 (120 bpm)
        trk.extend_from_slice(&[0x00, 0xff, 0x51, 0x03, 0x07, 0xa1, 0x20]);
        // note on C4 (60) vel 100 at delta 0
        trk.extend_from_slice(&[0x00, 0x90, 60, 100]);
        // note off C4 at delta 96
        trk.extend_from_slice(&[0x60, 0x80, 60, 0]);
        // note on E4 (64) at delta 0
        trk.extend_from_slice(&[0x00, 0x90, 64, 90]);
        // note off E4 at delta 96
        trk.extend_from_slice(&[0x60, 0x80, 64, 0]);
        // end of track
        trk.extend_from_slice(&[0x00, 0xff, 0x2f, 0x00]);
        d.extend_from_slice(b"MTrk");
        d.extend_from_slice(&(trk.len() as u32).to_be_bytes());
        d.extend_from_slice(&trk);
        d
    }

    #[test]
    fn parses_a_minimal_file() {
        let mf = parse(&tiny_smf()).expect("parses");
        assert_eq!(mf.ticks_per_quarter, 96);
        assert!((mf.bpm() - 120.0).abs() < 0.01);
        assert_eq!(mf.notes.len(), 2);
        assert_eq!(mf.notes[0].note, 60);
        assert_eq!(mf.notes[0].start_tick, 0);
        assert_eq!(mf.notes[0].duration_ticks, 96);
        assert_eq!(mf.notes[0].velocity, 100);
        assert_eq!(mf.notes[1].note, 64);
        assert_eq!(mf.notes[1].start_tick, 96);
    }

    #[test]
    fn note_on_zero_velocity_is_note_off() {
        let mut d = Vec::new();
        d.extend_from_slice(b"MThd");
        d.extend_from_slice(&6u32.to_be_bytes());
        d.extend_from_slice(&0u16.to_be_bytes());
        d.extend_from_slice(&1u16.to_be_bytes());
        d.extend_from_slice(&96u16.to_be_bytes());
        let mut trk = Vec::new();
        trk.extend_from_slice(&[0x00, 0x90, 60, 80]); // on
        trk.extend_from_slice(&[0x30, 0x90, 60, 0]); // off via vel 0 (running status)
        trk.extend_from_slice(&[0x00, 0xff, 0x2f, 0x00]);
        d.extend_from_slice(b"MTrk");
        d.extend_from_slice(&(trk.len() as u32).to_be_bytes());
        d.extend_from_slice(&trk);
        let mf = parse(&d).expect("parses");
        assert_eq!(mf.notes.len(), 1);
        assert_eq!(mf.notes[0].duration_ticks, 48);
    }

    #[test]
    fn rejects_non_midi() {
        assert!(parse(b"not a midi file at all").is_err());
    }
}
