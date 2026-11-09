//! Duplex, single-peer LAN transport. Audio processing remains on the server.
mod audio;
pub mod diagnostics;
pub mod playout;
pub mod protocol;
mod transport;

#[cfg(test)]
mod tests;

use crate::pipeline::AudioPipeline;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering},
    Arc,
};
use std::thread::JoinHandle;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LanRole {
    #[default]
    Local,
    Client,
    Server,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaptureBackend {
    #[default]
    WasapiShared,
    WasapiExclusive,
    Asio,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServerBridge {
    RsbAsio,
    #[default]
    SoundBridgeAsio,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct LanConfig {
    pub role: LanRole,
    pub server_ip: String,
    pub port: u16,
    pub capture_backend: CaptureBackend,
    pub input_device: String,
    pub asio_driver: String,
    pub input_channel: u16,
    pub return_device: String,
    pub buffer_size: u32,
    pub input_latency_ms: f32,
    pub return_latency_ms: f32,
    pub receive_buffer_ms: f32,
    pub bridge: ServerBridge,
    pub mic_gain: f32,
    pub return_gain: f32,
    pub mic_muted: bool,
    pub return_muted: bool,
}

impl Default for LanConfig {
    fn default() -> Self {
        Self {
            role: LanRole::Local,
            server_ip: "192.168.1.100".into(),
            port: 40100,
            capture_backend: CaptureBackend::WasapiShared,
            input_device: String::new(),
            asio_driver: String::new(),
            input_channel: 0,
            return_device: String::new(),
            buffer_size: 256,
            input_latency_ms: 0.0,
            return_latency_ms: 0.0,
            receive_buffer_ms: 5.0,
            bridge: ServerBridge::SoundBridgeAsio,
            mic_gain: 1.0,
            return_gain: 1.0,
            mic_muted: false,
            return_muted: false,
        }
    }
}

impl LanConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.port == 0 {
            return Err("Choose a UDP port from 1 to 65535".into());
        }
        if !self.receive_buffer_ms.is_finite() || !(2.5..=30.0).contains(&self.receive_buffer_ms) {
            return Err("Receive buffer must be between 2.5 and 30 ms".into());
        }
        if !(32..=4096).contains(&self.buffer_size) || self.input_channel >= 64 {
            return Err("Invalid audio buffer size or microphone channel".into());
        }
        for latency in [self.input_latency_ms, self.return_latency_ms] {
            if !latency.is_finite() || !(0.0..=500.0).contains(&latency) {
                return Err("Invalid device latency".into());
            }
        }
        if self.role == LanRole::Client {
            let ip = self
                .server_ip
                .parse::<std::net::Ipv4Addr>()
                .map_err(|_| "Enter the server's IPv4 address")?;
            if ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() {
                return Err("Enter a unicast server IPv4 address".into());
            }
            if self.return_device.is_empty() {
                return Err("Select the virtual cable playback device".into());
            }
            if self.capture_backend == CaptureBackend::Asio {
                if self.asio_driver.is_empty() {
                    return Err("Select an ASIO microphone driver".into());
                }
            } else if self.input_device.is_empty() {
                return Err("Select a microphone device".into());
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct LanStats {
    pub connected: AtomicBool,
    pub generation: AtomicU64,
    pub rtt_us: AtomicU64,
    pub fill_frames: AtomicU64,
    pub lost: AtomicU64,
    pub late: AtomicU64,
    pub duplicates: AtomicU64,
    pub invalid: AtomicU64,
    pub underruns: AtomicU64,
    pub overruns: AtomicU64,
    pub received: AtomicU64,
    pub sent: AtomicU64,
    pub drift_ppm: AtomicI32,
    pub mic_peak: AtomicU32,
    pub return_peak: AtomicU32,
    pub mic_gain: AtomicU32,
    pub return_gain: AtomicU32,
    pub mic_muted: AtomicBool,
    pub return_muted: AtomicBool,
    pub faulted: AtomicBool,
    pub message: Mutex<String>,
}

impl LanStats {
    pub fn new(config: &LanConfig) -> Arc<Self> {
        let stats = Arc::new(Self::default());
        stats.update_controls(config);
        stats
    }
    pub fn update_controls(&self, config: &LanConfig) {
        self.mic_gain.store(
            protocol::finite(config.mic_gain).clamp(0.0, 2.0).to_bits(),
            Ordering::Relaxed,
        );
        self.return_gain.store(
            protocol::finite(config.return_gain)
                .clamp(0.0, 2.0)
                .to_bits(),
            Ordering::Relaxed,
        );
        self.mic_muted.store(config.mic_muted, Ordering::Relaxed);
        self.return_muted
            .store(config.return_muted, Ordering::Relaxed);
    }
    pub fn status(&self, message: impl Into<String>) {
        *self.message.lock() = message.into();
    }
    pub fn fail(&self, message: impl Into<String>) {
        self.faulted.store(true, Ordering::Release);
        self.status(message);
    }
    pub(crate) fn new_generation(&self, connected: bool) {
        self.connected.store(false, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.fill_frames.store(0, Ordering::Relaxed);
        self.connected.store(connected, Ordering::Release);
    }
}

/// Owns all sockets, endpoint handles and workers for one user-started session.
pub struct LanSession {
    pub stats: Arc<LanStats>,
    client_audio: Option<audio::ClientAudio>,
    server_audio: Option<JoinHandle<()>>,
    transport: transport::Transport,
}

impl LanSession {
    pub fn start_client(config: &LanConfig) -> Result<Self, String> {
        config.validate()?;
        let stats = LanStats::new(config);
        let (transport, io) = transport::Transport::start(config, Arc::clone(&stats))?;
        let client_audio = audio::ClientAudio::start(config, io, Arc::clone(&stats))?;
        Ok(Self {
            stats,
            client_audio: Some(client_audio),
            server_audio: None,
            transport,
        })
    }
    pub fn start_server(
        config: &LanConfig,
        a: &mut AudioPipeline,
        b: &mut AudioPipeline,
    ) -> Result<Self, String> {
        config.validate()?;
        let stats = LanStats::new(config);
        let (transport, io) = transport::Transport::start(config, Arc::clone(&stats))?;
        let server_audio = audio::start_server(
            config,
            a,
            b,
            io,
            Arc::clone(&stats),
            Arc::clone(&transport.running),
        )?;
        Ok(Self {
            stats,
            client_audio: None,
            server_audio: Some(server_audio),
            transport,
        })
    }
}

impl Drop for LanSession {
    fn drop(&mut self) {
        self.client_audio.take();
        self.transport.running.store(false, Ordering::Release);
        if let Some(worker) = self.server_audio.take() {
            let _ = worker.join();
        }
        self.stats.new_generation(false);
    }
}
