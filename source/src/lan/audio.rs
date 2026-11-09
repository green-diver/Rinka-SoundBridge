use super::{
    playout::{Packetizer, Playout},
    protocol::{finite, FRAMES, RATE},
    transport::AudioIo,
    CaptureBackend, LanConfig, LanStats, ServerBridge,
};
use crate::{
    asio_host::{self, get_dsp_rsb_bus_a_out, get_dsp_rsb_bus_b_in, AsioStreamHandle},
    pipeline::{get_wasapi_devices, AudioPipeline},
    wasapi_exclusive::{FlexAsioEndpointOptions, WasapiFlexAsioPair},
    win32::{boost_audio_thread, sleep_high_res},
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) struct ClientAudio {
    watchdog_running: Arc<AtomicBool>,
    watchdog: Option<JoinHandle<()>>,
    _wasapi_capture: Option<WasapiFlexAsioPair>,
    _asio_capture: Option<AsioStreamHandle>,
    _playback: WasapiFlexAsioPair,
}

impl ClientAudio {
    pub fn start(config: &LanConfig, io: AudioIo, stats: Arc<LanStats>) -> Result<Self, String> {
        let (inputs, outputs) = get_wasapi_devices();
        let output = outputs
            .iter()
            .find(|d| d.name == config.return_device)
            .ok_or("Return playback device not found")?;
        let mut receiver = io.rx;
        let mut playout = Playout::new(config.receive_buffer_ms, Arc::clone(&stats))?;
        let return_stats = Arc::clone(&stats);
        let capture_ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let return_ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let played = Arc::clone(&return_ticks);
        let output_options = FlexAsioEndpointOptions {
            set_latency_ms: Some(config.return_latency_ms),
            wasapi_exclusive: false,
            ..Default::default()
        };
        let playback = WasapiFlexAsioPair::try_start_with_options(
            None,
            Some(output.index),
            RATE,
            config.buffer_size,
            FlexAsioEndpointOptions::default(),
            output_options,
            |_, _, _| {},
            move |data, channels, _| {
                played.fetch_add(1, Ordering::Relaxed);
                playout.receive(&mut receiver);
                playout.render(data, channels);
                let gain = if return_stats.return_muted.load(Ordering::Relaxed) {
                    0.0
                } else {
                    f32::from_bits(return_stats.return_gain.load(Ordering::Relaxed))
                };
                let mut peak = 0.0f32;
                for sample in data.iter_mut() {
                    *sample = (finite(*sample) * gain).clamp(-1.0, 1.0);
                    peak = peak.max(sample.abs());
                }
                return_stats
                    .return_peak
                    .store(peak.to_bits(), Ordering::Relaxed);
            },
        )?;
        if playback.active_sample_rate != RATE {
            return Err("Return device must run at 48000 Hz".into());
        }
        let mut packetizer = Packetizer::new(io.tx, 1, Arc::clone(&stats));
        let capture_stats = Arc::clone(&stats);
        let captured = Arc::clone(&capture_ticks);
        let channel = if config.capture_backend == CaptureBackend::Asio {
            0
        } else {
            config.input_channel as usize
        };
        let mut capture = move |data: &[f32], channels: usize, frames: usize| {
            captured.fetch_add(1, Ordering::Relaxed);
            packetizer.set_quantum(frames);
            if frames == 0 || channel >= channels {
                if !capture_stats.faulted.load(Ordering::Relaxed) {
                    capture_stats.fail("Capture channel unavailable or ASIO rate/reset changed; set 48000 Hz and reconnect");
                }
                return;
            }
            let gain = if capture_stats.mic_muted.load(Ordering::Relaxed) {
                0.0
            } else {
                f32::from_bits(capture_stats.mic_gain.load(Ordering::Relaxed))
            };
            let mut peak = 0.0f32;
            for frame in data.chunks_exact(channels).take(frames) {
                let mono = finite(frame[channel]) * gain;
                peak = peak.max(mono.abs());
                packetizer.push(mono, mono);
            }
            capture_stats
                .mic_peak
                .store(peak.to_bits(), Ordering::Relaxed);
        };
        let (wasapi_capture, asio_capture) = if config.capture_backend == CaptureBackend::Asio {
            // The ASIO host selects the requested channel and supplies a mono slice.
            let source_stats = Arc::clone(&stats);
            let mut tx = capture;
            let source_channel = config.input_channel;
            let handle = AsioStreamHandle::start_capture(
                &config.asio_driver,
                RATE,
                config.buffer_size,
                source_channel,
                Box::new(move |data, frames| {
                    if frames == 0 {
                        source_stats.fail("ASIO rate/reset changed; set 48000 Hz and reconnect");
                        return;
                    }
                    tx(data, 1, frames);
                }),
            )?;
            if handle.active_sample_rate != RATE {
                return Err("ASIO microphone must run at 48000 Hz".into());
            }
            (None, Some(handle))
        } else {
            let input = inputs
                .iter()
                .find(|d| d.name == config.input_device)
                .ok_or("Microphone device not found")?;
            if channel >= input.max_channels as usize {
                return Err("Selected microphone channel is unavailable".into());
            }
            let options = FlexAsioEndpointOptions {
                set_latency_ms: Some(config.input_latency_ms),
                wasapi_exclusive: config.capture_backend == CaptureBackend::WasapiExclusive,
                channels: if channel == 0 {
                    0
                } else {
                    input.max_channels.min(16)
                },
                ..Default::default()
            };
            let handle = WasapiFlexAsioPair::try_start_with_options(
                Some(input.index),
                None,
                RATE,
                config.buffer_size,
                options,
                FlexAsioEndpointOptions::default(),
                move |data, channels, frames| capture(data, channels, frames),
                |_, _, _| {},
            )?;
            if handle.active_sample_rate != RATE {
                return Err("Microphone must run at 48000 Hz".into());
            }
            (Some(handle), None)
        };
        let watchdog_running = Arc::new(AtomicBool::new(true));
        let watch_flag = Arc::clone(&watchdog_running);
        let watchdog = thread::Builder::new().name("rinka-lan-devices".into()).spawn(move || {
            let mut previous = [0; 2];
            let mut changed = [Instant::now(); 2];
            while watch_flag.load(Ordering::Acquire) {
                for (index, ticks) in [&capture_ticks, &return_ticks].iter().enumerate() {
                    let current = ticks.load(Ordering::Relaxed);
                    if current != previous[index] { previous[index] = current; changed[index] = Instant::now(); }
                    else if changed[index].elapsed() > Duration::from_secs(2) && !stats.faulted.load(Ordering::Acquire) {
                        stats.fail(if index == 0 { "Microphone stream stopped; check the device and reconnect" }
                            else { "Return playback stream stopped; check the cable device and reconnect" });
                    }
                }
                thread::sleep(Duration::from_millis(100));
            }
        }).map_err(|e| e.to_string())?;
        Ok(Self {
            watchdog_running,
            watchdog: Some(watchdog),
            _wasapi_capture: wasapi_capture,
            _asio_capture: asio_capture,
            _playback: playback,
        })
    }
}

impl Drop for ClientAudio {
    fn drop(&mut self) {
        self.watchdog_running.store(false, Ordering::Release);
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
    }
}

pub(crate) fn start_server(
    config: &LanConfig,
    a: &mut AudioPipeline,
    b: &mut AudioPipeline,
    io: AudioIo,
    stats: Arc<LanStats>,
    running: Arc<AtomicBool>,
) -> Result<JoinHandle<()>, String> {
    let rsb = config.bridge == ServerBridge::RsbAsio;
    let block = if rsb {
        a.rsb_asio_config.effective_buffer_size(a.buffer_size)
    } else {
        a.dsp_rsb_config.effective_buffer_size(a.buffer_size)
    };
    let (in_latency, out_latency) = if rsb {
        asio_host::compute_rsb_flexasio_reported_latency_samples(
            RATE,
            block,
            a.rsb_asio_config.set_in_latency_enabled,
            a.rsb_asio_config.in_latency_val,
            a.rsb_asio_config.set_out_latency_enabled,
            a.rsb_asio_config.out_latency_val,
        )
    } else {
        let cfg = &a.dsp_rsb_config;
        asio_host::compute_asio_reported_latency_samples(
            RATE,
            block,
            if cfg.adaptive_reported_latency {
                0.0
            } else {
                cfg.reported_in_ms
            },
            if cfg.adaptive_reported_latency {
                0.0
            } else {
                cfg.reported_out_ms
            },
        )
    };
    let base_cushion = if rsb {
        let cfg = &a.rsb_asio_config;
        let extra = if cfg.set_out_latency_enabled
            && cfg.out_latency_val > 0.0
            && cfg.out_latency_val < 0.5
        {
            cfg.out_latency_val.min(0.250) * RATE as f32
        } else {
            0.0
        };
        (0.024 * RATE as f32 + extra) as usize
    } else {
        (a.dsp_rsb_config.dsp_b_cushion_ms.clamp(20.0, 450.0) * RATE as f32 / 1000.0) as usize
    };
    let mut playout = Playout::new(config.receive_buffer_ms, Arc::clone(&stats))?;
    let mut line_a = a.prepare_lan(config.bridge);
    let mut line_b = b.prepare_lan(config.bridge);
    let bus_a = get_dsp_rsb_bus_a_out();
    let bus_b = get_dsp_rsb_bus_b_in();
    bus_a.sync_config(RATE, block, in_latency, out_latency);
    if rsb {
        bus_b.set_adaptive_flags_with_mult(true, true, 1.05);
        bus_b.set_adaptive_mode(3);
        bus_b.set_user_extra_buffer_ms(0.0);
    } else {
        let cfg = &a.dsp_rsb_config;
        bus_b.set_adaptive_flags_with_mult(
            cfg.adaptive_cushion,
            cfg.adaptive_reported_latency,
            cfg.gpu_stall_multiplier,
        );
        bus_b.set_adaptive_mode(cfg.adaptive_mode as u8);
        bus_b.set_user_extra_buffer_ms(cfg.adaptive_extra_ms);
    }
    bus_b.sync_config(RATE, block, 1, base_cushion as u32);
    bus_a.reset();
    bus_b.reset_with_cushion(base_cushion);
    // Guard the RSB driver's standalone fallback even before the first client connects.
    asio_host::mark_lan_server_active(true);
    thread::Builder::new()
        .name("rinka-lan-server-audio".into())
        .spawn(move || {
            let _ = boost_audio_thread();
            let mut receiver = io.rx;
            let mut packetizer = Packetizer::new(io.tx, 2, Arc::clone(&stats));
            let mut input = [0.0f32; FRAMES * 2];
            let mut output = [0.0f32; FRAMES * 2];
            let period = Duration::from_micros(2500);
            let mut deadline = Instant::now();
            let mut generation = stats.generation.load(Ordering::Acquire);
            let mut vc_active = false;
            while running.load(Ordering::Acquire) && !stats.faulted.load(Ordering::Acquire) {
                asio_host::mark_lan_server_active(true);
                if bus_a.get_config().0 != RATE || bus_b.get_config().0 != RATE {
                    stats.fail(
                        "Vonovox changed sample rate; select 48000 Hz and restart the LAN server",
                    );
                    break;
                }
                let next_generation = stats.generation.load(Ordering::Acquire);
                let next_vc = bus_b.is_vc_active();
                if generation != next_generation || vc_active != next_vc {
                    generation = next_generation;
                    vc_active = next_vc;
                    line_a.reset();
                    line_b.reset();
                    bus_a.reset();
                    bus_b.reset_with_cushion(base_cushion);
                }
                playout.receive(&mut receiver);
                playout.render(&mut input, 2);
                let processed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    line_a.input(&input);
                    if vc_active {
                        line_b.output(&mut output, rsb, base_cushion);
                    } else {
                        output.fill(0.0);
                    }
                }));
                if processed.is_err() {
                    stats.fail("Server audio processing failed; restart the LAN server");
                    break;
                }
                if !stats.connected.load(Ordering::Acquire) {
                    output.fill(0.0);
                }
                for frame in output.chunks_exact(2) {
                    packetizer.push(frame[0], frame[1]);
                }
                deadline += period;
                let now = Instant::now();
                if deadline > now {
                    sleep_high_res(deadline - now);
                } else if now.duration_since(deadline) > Duration::from_millis(20) {
                    stats.overruns.fetch_add(1, Ordering::Relaxed);
                    deadline = now; // Never flood the network with a stale catch-up burst.
                }
            }
            bus_a.mark_writer_stopped();
            bus_a.reset();
            bus_b.reset();
            asio_host::mark_lan_server_active(false);
            running.store(false, Ordering::Release);
        })
        .map_err(|e| {
            asio_host::mark_lan_server_active(false);
            e.to_string()
        })
}
