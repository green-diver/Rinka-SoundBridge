//! Version 1 LAN wire format. Header integers and PCM samples are little endian.
//! Audio datagrams are exactly one 120-frame block, mono upstream/stereo downstream.
pub const RATE: u32 = 48_000;
pub const FRAMES: usize = 120;
pub const HEADER: usize = 32;
pub const MAX_DATAGRAM: usize = HEADER + FRAMES * 2 * 4;
pub const MAGIC: &[u8; 4] = b"RSBL";
pub const VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Hello = 1,
    Welcome = 2,
    Audio = 3,
    Ping = 4,
    Pong = 5,
    Busy = 6,
    Bye = 7,
}

#[derive(Clone, Debug)]
pub struct Packet {
    pub kind: Kind,
    pub direction: u8,
    pub channels: u8,
    pub session: u64,
    pub sequence: u32,
    pub position: u64,
    /// Capture callback size, so a receiver can reserve for normal device bursts.
    pub quantum: u16,
    pub samples: [f32; FRAMES * 2],
    /// Local queue generation; never transmitted.
    pub generation: u64,
}

impl Packet {
    pub fn control(kind: Kind, session: u64, position: u64) -> Self {
        Self {
            kind,
            direction: 0,
            channels: 0,
            session,
            sequence: 0,
            position,
            quantum: 0,
            samples: [0.0; FRAMES * 2],
            generation: 0,
        }
    }

    pub fn audio(channels: u8) -> Self {
        Self {
            kind: Kind::Audio,
            channels,
            quantum: FRAMES as u16,
            direction: if channels == 1 { 1 } else { 2 },
            ..Self::control(Kind::Audio, 0, 0)
        }
    }

    pub fn encode(&self, out: &mut [u8; MAX_DATAGRAM]) -> usize {
        out[..HEADER].fill(0);
        out[..4].copy_from_slice(MAGIC);
        out[4] = VERSION;
        out[5] = self.kind as u8;
        out[6] = self.direction;
        out[7] = self.channels;
        out[8..16].copy_from_slice(&self.session.to_le_bytes());
        out[16..20].copy_from_slice(&self.sequence.to_le_bytes());
        out[20..28].copy_from_slice(&self.position.to_le_bytes());
        if self.kind != Kind::Audio {
            return HEADER;
        }
        out[28..30].copy_from_slice(&(FRAMES as u16).to_le_bytes());
        out[30..32].copy_from_slice(&self.quantum.to_le_bytes());
        let count = FRAMES * self.channels as usize;
        for (bytes, sample) in out[HEADER..HEADER + count * 4]
            .chunks_exact_mut(4)
            .zip(self.samples.iter())
        {
            bytes.copy_from_slice(&finite(*sample).to_le_bytes());
        }
        HEADER + count * 4
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() < HEADER || &bytes[..4] != MAGIC {
            return Err("Invalid LAN header");
        }
        if bytes[4] != VERSION {
            return Err("Incompatible LAN protocol version");
        }
        let kind = match bytes[5] {
            1 => Kind::Hello,
            2 => Kind::Welcome,
            3 => Kind::Audio,
            4 => Kind::Ping,
            5 => Kind::Pong,
            6 => Kind::Busy,
            7 => Kind::Bye,
            _ => return Err("Unknown packet type"),
        };
        let mut packet = Self::control(
            kind,
            u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            u64::from_le_bytes(bytes[20..28].try_into().unwrap()),
        );
        packet.sequence = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
        packet.direction = bytes[6];
        packet.channels = bytes[7];
        packet.quantum = u16::from_le_bytes(bytes[30..32].try_into().unwrap());
        let frames = u16::from_le_bytes(bytes[28..30].try_into().unwrap()) as usize;
        if kind == Kind::Audio {
            if !matches!((packet.direction, packet.channels), (1, 1) | (2, 2))
                || frames != FRAMES
                || !(FRAMES as u16..=4096).contains(&packet.quantum)
                || packet.position % FRAMES as u64 != 0
                || packet.position.checked_add(FRAMES as u64).is_none()
                || bytes.len() != HEADER + frames * packet.channels as usize * 4
            {
                return Err("Invalid audio layout");
            }
            for (sample, data) in packet
                .samples
                .iter_mut()
                .zip(bytes[HEADER..].chunks_exact(4))
            {
                *sample = finite(f32::from_le_bytes(data.try_into().unwrap()));
            }
        } else if bytes.len() != HEADER
            || frames != 0
            || packet.channels != 0
            || packet.direction != 0
            || packet.quantum != 0
        {
            return Err("Invalid control packet");
        }
        Ok(packet)
    }
}

pub fn finite(sample: f32) -> f32 {
    if sample.is_finite() {
        sample
    } else {
        0.0
    }
}
