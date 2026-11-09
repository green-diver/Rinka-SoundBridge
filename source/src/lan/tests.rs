use super::*;
use playout::{DriftController, Packetizer, Playout};
use protocol::{Kind, Packet, FRAMES, HEADER, MAX_DATAGRAM};
use std::{
    net::UdpSocket,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

fn stats() -> Arc<LanStats> {
    LanStats::new(&LanConfig::default())
}
fn packet(index: u64, channels: u8) -> Packet {
    let mut p = Packet::audio(channels);
    p.position = index * FRAMES as u64;
    p.sequence = index as u32;
    p.samples.fill(0.25);
    p
}
fn port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !predicate() {
        assert!(Instant::now() < deadline, "Condition timed out");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn packet_roundtrip_and_sequence_wrap() {
    for channels in [1, 2] {
        for sequence in [0, u32::MAX, 1] {
            let mut p = packet(123, channels);
            p.session = 987654321;
            p.sequence = sequence;
            p.samples[0] = f32::NAN;
            p.samples[1] = f32::INFINITY;
            let mut bytes = [0; MAX_DATAGRAM];
            let size = p.encode(&mut bytes);
            assert!(size < 1200);
            let decoded = Packet::decode(&bytes[..size]).unwrap();
            assert_eq!(
                (decoded.position, decoded.sequence, decoded.session),
                (p.position, sequence, p.session)
            );
            assert_eq!(&decoded.samples[..2], &[0.0, 0.0]);
        }
    }
}

#[test]
fn malformed_datagrams_are_rejected_without_panics() {
    let mut bytes = [0; MAX_DATAGRAM];
    let size = packet(0, 2).encode(&mut bytes);
    for n in 0..size {
        assert!(Packet::decode(&bytes[..n]).is_err());
    }
    for (index, value) in [(4, 99), (5, 99), (6, 1), (7, 255), (28, 1), (30, 1)] {
        let mut bad = bytes;
        bad[index] = value;
        assert!(Packet::decode(&bad[..size]).is_err());
    }
    let mut bad = bytes;
    bad[20..28].copy_from_slice(&(u64::MAX - u64::MAX % FRAMES as u64).to_le_bytes());
    assert!(Packet::decode(&bad[..size]).is_err());
    let control = Packet::control(Kind::Ping, 5, 6);
    assert_eq!(control.encode(&mut bytes), HEADER);
    assert!(Packet::decode(&bytes[..HEADER + 1]).is_err());
    use rand::{RngCore, SeedableRng};
    let mut rng = rand::rngs::StdRng::seed_from_u64(12);
    for _ in 0..2000 {
        rng.fill_bytes(&mut bytes);
        assert!(Packet::decode(&bytes).is_err());
    }
}

#[test]
fn packetization_is_independent_of_callback_size_and_resets_on_reconnect() {
    let stats = stats();
    let (producer, mut consumer) = rtrb::RingBuffer::new(128);
    let mut tx = Packetizer::new(producer, 1, Arc::clone(&stats));
    let mut count = 0;
    for frames in [32, 64, 128, 256, 512, 1024, 257, 480, 960] {
        for _ in 0..frames {
            tx.push(count as f32 / 10000.0, 0.0);
            count += 1;
        }
    }
    let mut read = 0;
    while let Ok(packet) = consumer.pop() {
        assert_eq!(packet.position, read as u64);
        for sample in &packet.samples[..FRAMES] {
            assert_eq!(*sample, read as f32 / 10000.0);
            read += 1;
        }
    }
    assert_eq!(read, count / FRAMES * FRAMES);
    stats.new_generation(true);
    for _ in 0..FRAMES {
        tx.push(0.8, 0.0);
    }
    let first = consumer.pop().unwrap();
    assert_eq!(
        (first.position, first.sequence, first.generation),
        (0, 0, 1)
    );
    assert!(first.samples[..FRAMES].iter().all(|s| *s == 0.8));
}

#[test]
fn jitter_reorders_discards_duplicates_and_conceals_loss() {
    let stats = stats();
    let mut player = Playout::new(2.5, Arc::clone(&stats)).unwrap();
    for index in [1, 0, 2, 2, 4, 5, 6] {
        player.push(packet(index, 1));
    }
    let mut output = [0.0; FRAMES * 2];
    let mut heard = false;
    for _ in 0..20 {
        player.render(&mut output, 2);
        heard |= output.iter().any(|s| s.abs() > 0.1);
        assert!(output.iter().all(|s| s.is_finite() && s.abs() < 1.0));
    }
    player.push(packet(0, 1));
    assert!(heard);
    assert_eq!(stats.duplicates.load(Ordering::Relaxed), 1);
    assert!(stats.lost.load(Ordering::Relaxed) >= 1);
    // A silent tail must stay silent rather than periodically replaying its last sample.
    assert!(output.iter().all(|s| s.abs() < 1e-6));
}

#[test]
fn drift_controller_tracks_both_clocks_for_a_simulated_hour() {
    for ppm in [-300.0, 300.0] {
        let mut controller = DriftController::default();
        let mut fill = 240.0;
        let mut min_fill = fill;
        let mut max_fill = fill;
        let source_ratio = 1.0 + ppm / 1e6;
        let mut ratio = 1.0;
        for step in 0..1_440_000 {
            let jitter = [2.0, -1.0, -1.0][step % 3];
            ratio = controller.update(fill + jitter, 240.0);
            fill += FRAMES as f64 * (source_ratio - ratio);
            min_fill = f64::min(min_fill, fill);
            max_fill = f64::max(max_fill, fill);
        }
        assert!(
            min_fill > 120.0 && max_fill < 360.0,
            "{ppm} ppm fill range {min_fill}..{max_fill}"
        );
        assert!((fill - 240.0).abs() < 3.0, "Final fill {fill}");
        assert!((ratio - source_ratio).abs() < 5e-6, "Final ratio {ratio}");
    }
}

#[test]
fn resampler_survives_jitter_and_clock_drift_without_growing_latency() {
    for ppm in [-300.0, 300.0] {
        let stats = stats();
        let mut player = Playout::new(5.0, Arc::clone(&stats)).unwrap();
        let mut produced = 0u64;
        let mut sender_clock = 480.0;
        let mut output = [0.0; FRAMES * 2];
        for step in 0..12000 {
            sender_clock += FRAMES as f64 * (1.0 + ppm / 1e6);
            // Deterministic 0..2.5 ms network jitter with catch-up on the next tick.
            if step % 9 != 0 {
                while (produced + 1) as f64 * FRAMES as f64 <= sender_clock {
                    player.push(packet(produced, 2));
                    produced += 1;
                }
            }
            player.render(&mut output, 2);
            assert!(output.iter().all(|sample| sample.is_finite()));
        }
        assert_eq!(
            stats.underruns.load(Ordering::Relaxed),
            0,
            "{ppm} ppm underruns"
        );
        assert!(stats.fill_frames.load(Ordering::Relaxed) < 1200);
    }
}

#[test]
fn legacy_configuration_defaults_to_local_and_preserves_settings() {
    let config: crate::config::AppConfig =
        serde_json::from_str(include_str!("../../rinka_soundbridge_config.json")).unwrap();
    assert_eq!(config.lan.role, LanRole::Local);
    let mut config = config;
    let old_a = serde_json::to_value(&config.line_a).unwrap();
    config.lan.role = LanRole::Client;
    let restored: crate::config::AppConfig =
        serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
    assert_eq!(serde_json::to_value(restored.line_a).unwrap(), old_a);
    assert_eq!(restored.lan.role, LanRole::Client);
    config.lan.receive_buffer_ms = f32::NAN;
    assert!(config.lan.validate().is_err());
}

#[test]
fn udp_sessions_pin_peer_reject_stale_audio_and_recover_after_restart() {
    let config = LanConfig {
        role: LanRole::Server,
        port: port(),
        ..Default::default()
    };
    let server_stats = LanStats::new(&config);
    let (server, mut server_io) =
        transport::Transport::start(&config, Arc::clone(&server_stats)).unwrap();
    let client_config = LanConfig {
        role: LanRole::Client,
        server_ip: "127.0.0.1".into(),
        ..config.clone()
    };
    let client_stats = LanStats::new(&client_config);
    let (client, mut client_io) =
        transport::Transport::start(&client_config, Arc::clone(&client_stats)).unwrap();
    until(|| client_stats.connected.load(Ordering::Acquire));
    let impostor = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut bytes = [0; MAX_DATAGRAM];
    let size = packet(0, 1).encode(&mut bytes);
    impostor
        .send_to(&bytes[..size], (std::net::Ipv4Addr::LOCALHOST, config.port))
        .unwrap();
    std::thread::sleep(Duration::from_millis(10));
    assert!(server_io.rx.pop().is_err());
    let busy_stats = LanStats::new(&client_config);
    let (_busy, _busy_io) =
        transport::Transport::start(&client_config, Arc::clone(&busy_stats)).unwrap();
    until(|| busy_stats.message.lock().contains("busy"));
    assert!(!busy_stats.connected.load(Ordering::Relaxed));
    drop(_busy);
    let mut good = packet(0, 1);
    good.generation = client_stats.generation.load(Ordering::Acquire);
    client_io.tx.push(good).unwrap();
    until(|| server_stats.received.load(Ordering::Relaxed) > 0);
    assert_eq!(server_io.rx.pop().unwrap().position, 0);
    drop(server);
    until(|| !client_stats.connected.load(Ordering::Acquire));
    let next_stats = LanStats::new(&config);
    let (_replacement, _replacement_io) = transport::Transport::start(&config, next_stats).unwrap();
    until(|| client_stats.connected.load(Ordering::Acquire));
    assert!(client_stats.generation.load(Ordering::Acquire) >= 3);
    drop(client);
}

#[test]
fn transport_echo_measures_both_receive_buffers() {
    let port = port();
    let server = std::thread::spawn(move || diagnostics::echo_server(port, Duration::from_secs(4)));
    std::thread::sleep(Duration::from_millis(40));
    let report = diagnostics::probe("127.0.0.1", port, Duration::from_secs(3)).unwrap();
    server.join().unwrap().unwrap();
    println!("Echo: {report:?}");
    assert!(report.measurements >= 3);
    // This is a scheduling-sensitive localhost smoke check, not a physical-LAN latency claim.
    assert!(report.median_ms < 100.0);
}

#[test]
fn silent_peer_times_out_after_three_seconds() {
    let config = LanConfig {
        role: LanRole::Server,
        port: port(),
        ..Default::default()
    };
    let s = LanStats::new(&config);
    let (_server, _io) = transport::Transport::start(&config, Arc::clone(&s)).unwrap();
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut bytes = [0; MAX_DATAGRAM];
    let count = Packet::control(Kind::Hello, 123, 0).encode(&mut bytes);
    peer.send_to(
        &bytes[..count],
        (std::net::Ipv4Addr::LOCALHOST, config.port),
    )
    .unwrap();
    until(|| s.connected.load(Ordering::Acquire));
    let start = Instant::now();
    while s.connected.load(Ordering::Acquire) {
        assert!(start.elapsed() < Duration::from_secs(4));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(start.elapsed() >= Duration::from_millis(2900));
}

#[test]
fn client_waits_for_a_server_that_starts_later() {
    let config = LanConfig {
        role: LanRole::Client,
        server_ip: "127.0.0.1".into(),
        port: port(),
        ..Default::default()
    };
    let s = LanStats::new(&config);
    let (_client, _io) = transport::Transport::start(&config, Arc::clone(&s)).unwrap();
    std::thread::sleep(Duration::from_millis(650));
    assert!(!s.faulted.load(Ordering::Acquire), "{}", s.message.lock());
    let server_config = LanConfig {
        role: LanRole::Server,
        ..config
    };
    let (_server, _server_io) = transport::Transport::start(&server_config, stats()).unwrap();
    until(|| s.connected.load(Ordering::Acquire));
}

#[test]
fn hardware_callback_bursts_keep_the_receive_reserve() {
    for (capture, render) in [
        (480, 120),
        (120, 480),
        (480, 480),
        (256, 120),
        (1024, 480),
        (4096, 120),
    ] {
        let stats = stats();
        stats.new_generation(true);
        let (producer, mut consumer) = rtrb::RingBuffer::new(40);
        let mut tx = Packetizer::new(producer, 1, Arc::clone(&stats));
        tx.set_quantum(capture);
        let mut player = Playout::new(5.0, Arc::clone(&stats)).unwrap();
        let mut output = vec![0.0; render * 2];
        let mut next_capture = 0;
        let mut heard = false;
        for time in (0..48000 * 10).step_by(render) {
            while next_capture <= time {
                for _ in 0..capture {
                    tx.push(0.25, 0.25);
                }
                next_capture += capture;
            }
            player.receive(&mut consumer);
            player.render(&mut output, 2);
            heard |= output.iter().any(|v| v.abs() > 0.1);
        }
        assert!(heard, "No audio for {capture}/{render}");
        assert_eq!(
            stats.underruns.load(Ordering::Relaxed),
            0,
            "Underruns for {capture}/{render}"
        );
        assert_eq!(
            stats.overruns.load(Ordering::Relaxed),
            0,
            "Overruns for {capture}/{render}"
        );
    }
}

#[test]
fn stopped_server_settings_cannot_restart_local_audio() {
    let mut pipeline = crate::pipeline::AudioPipeline::new("Line A");
    pipeline.lan_routing = true;
    pipeline.restart_stream();
    assert_eq!(pipeline.status_message, "Ready");
    assert!(!pipeline.streams_running());
    pipeline.set_enabled(false);
    pipeline.set_enabled(true);
    assert_eq!(pipeline.status_message, "Ready");
    pipeline.stop_lan();
    pipeline.restart_stream();
    assert_eq!(pipeline.status_message, "Ready");
}

#[test]
fn both_server_bridges_process_audio_without_local_devices() {
    use crate::{
        asio_host::{get_dsp_rsb_bus_a_out, get_dsp_rsb_bus_b_in},
        pipeline::AudioPipeline,
        win32::sleep_high_res,
    };
    for bridge in [ServerBridge::RsbAsio, ServerBridge::SoundBridgeAsio] {
        let config = LanConfig {
            role: LanRole::Server,
            port: port(),
            bridge,
            ..Default::default()
        };
        let mut a = AudioPipeline::new("Line A");
        let mut b = AudioPipeline::new("Line B");
        let server = LanSession::start_server(&config, &mut a, &mut b).unwrap();
        assert!(a.lan_managed && b.lan_managed);
        assert!(a.input_device_name.is_none() && b.output_device_name.is_none());
        let mock_running = Arc::new(AtomicBool::new(true));
        let mock_flag = Arc::clone(&mock_running);
        // Stand in for Vonovox's ASIO callback: copy input to output through the actual buses.
        // This exercises both server read policies without opening devices or injecting a GPU probe.
        let mock = std::thread::spawn(move || {
            let mut left = [0.0; 256];
            let mut right = [0.0; 256];
            while mock_flag.load(Ordering::Acquire) {
                get_dsp_rsb_bus_b_in().mark_vc_heartbeat(true);
                if get_dsp_rsb_bus_a_out().try_read_exact_stereo_into(&mut left, &mut right) {
                    let cushion = get_dsp_rsb_bus_b_in().get_dsp_b_params().0;
                    get_dsp_rsb_bus_b_in().write_stereo_dsp_b(&left, &right, cushion);
                } else {
                    sleep_high_res(Duration::from_micros(500));
                }
            }
            get_dsp_rsb_bus_b_in().mark_vc_heartbeat(false);
        });
        let cc = LanConfig {
            role: LanRole::Client,
            server_ip: "127.0.0.1".into(),
            ..config.clone()
        };
        let cs = LanStats::new(&cc);
        let (_client, mut io) = transport::Transport::start(&cc, Arc::clone(&cs)).unwrap();
        until(|| cs.connected.load(Ordering::Acquire));
        let mut player = Playout::new(5.0, Arc::clone(&cs)).unwrap();
        let mut tx = Packetizer::new(io.tx, 1, Arc::clone(&cs));
        let mut heard = false;
        let mut output = [0.0; FRAMES * 2];
        let mut sample = 0;
        let mut deadline = Instant::now();
        for _ in 0..300 {
            for _ in 0..FRAMES {
                let value = (sample as f32 * std::f32::consts::TAU * 1000.0 / 48000.0).sin() * 0.15;
                sample += 1;
                tx.push(value, value);
            }
            player.receive(&mut io.rx);
            player.render(&mut output, 2);
            heard |= output.iter().any(|value| value.abs() > 0.03);
            deadline += Duration::from_micros(2500);
            if deadline > Instant::now() {
                sleep_high_res(deadline - Instant::now());
            }
        }
        mock_running.store(false, Ordering::Release);
        mock.join().unwrap();
        assert!(
            !server.stats.faulted.load(Ordering::Acquire),
            "{}",
            server.stats.message.lock()
        );
        drop(server);
        a.stop_lan();
        b.stop_lan();
        assert!(heard, "No return audio from {bridge:?}");
    }
}
