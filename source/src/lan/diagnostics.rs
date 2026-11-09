//! Audio-device-free transport echo harness. Measures both network receive buffers,
//! packetization, worker scheduling and resampling, excluding Vonovox and ASIO cushions.
use super::{
    playout::{Packetizer, Playout},
    protocol::FRAMES,
    transport::Transport,
    LanConfig, LanRole, LanStats,
};
use crate::win32::{boost_audio_thread, sleep_high_res};
use std::{
    collections::VecDeque,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct EchoReport {
    pub measurements: usize,
    pub median_ms: f64,
    pub p95_ms: f64,
    pub lost: u64,
    pub underruns: u64,
}

pub fn echo_server(port: u16, duration: Duration) -> Result<(), String> {
    let config = LanConfig {
        role: LanRole::Server,
        port,
        ..Default::default()
    };
    let stats = LanStats::new(&config);
    let (_transport, mut io) = Transport::start(&config, Arc::clone(&stats))?;
    let mut playout = Playout::new(config.receive_buffer_ms, Arc::clone(&stats))?;
    let mut tx = Packetizer::new(io.tx, 2, Arc::clone(&stats));
    let mut block = [0.0; FRAMES * 2];
    let _ = boost_audio_thread();
    let start = Instant::now();
    let mut deadline = start;
    while start.elapsed() < duration && !stats.faulted.load(Ordering::Acquire) {
        playout.receive(&mut io.rx);
        playout.render(&mut block, 2);
        for frame in block.chunks_exact(2) {
            tx.push(frame[0], frame[1]);
        }
        pace(&mut deadline);
    }
    if stats.faulted.load(Ordering::Acquire) {
        Err(stats.message.lock().clone())
    } else {
        Ok(())
    }
}

pub fn probe(server_ip: &str, port: u16, duration: Duration) -> Result<EchoReport, String> {
    let config = LanConfig {
        role: LanRole::Client,
        server_ip: server_ip.into(),
        port,
        ..Default::default()
    };
    let stats = LanStats::new(&config);
    let (_transport, mut io) = Transport::start(&config, Arc::clone(&stats))?;
    let mut playout = Playout::new(config.receive_buffer_ms, Arc::clone(&stats))?;
    let mut tx = Packetizer::new(io.tx, 1, Arc::clone(&stats));
    let _ = boost_audio_thread();
    let mut output = [0.0; FRAMES * 2];
    let mut waiting = VecDeque::new();
    let mut measurements = Vec::<f64>::new();
    let mut high = false;
    let start = Instant::now();
    let mut deadline = start;
    let mut next_pulse = start + Duration::from_secs(1);
    while start.elapsed() < duration && !stats.faulted.load(Ordering::Acquire) {
        let pulse = stats.connected.load(Ordering::Acquire) && Instant::now() >= next_pulse;
        if pulse {
            waiting.push_back(Instant::now());
            next_pulse = Instant::now() + Duration::from_millis(500);
        }
        for i in 0..FRAMES {
            let sample = if pulse && i < 24 { 0.8 } else { 0.0 };
            tx.push(sample, sample);
        }
        playout.receive(&mut io.rx);
        playout.render(&mut output, 2);
        for frame in output.chunks_exact(2) {
            let above = frame[0] > 0.3;
            if above && !high {
                if let Some(sent) = waiting.pop_front() {
                    measurements.push(sent.elapsed().as_secs_f64() * 1000.0);
                }
            }
            high = above;
        }
        while waiting
            .front()
            .is_some_and(|sent| sent.elapsed() > Duration::from_millis(400))
        {
            waiting.pop_front();
        }
        pace(&mut deadline);
    }
    if measurements.is_empty() {
        return Err(format!("No echo received: {}", stats.message.lock()));
    }
    measurements.sort_by(f64::total_cmp);
    Ok(EchoReport {
        measurements: measurements.len(),
        median_ms: measurements[measurements.len() / 2],
        p95_ms: measurements[(measurements.len() * 95 / 100).min(measurements.len() - 1)],
        lost: stats.lost.load(Ordering::Relaxed),
        underruns: stats.underruns.load(Ordering::Relaxed),
    })
}

fn pace(deadline: &mut Instant) {
    *deadline += Duration::from_micros(2500);
    let now = Instant::now();
    if *deadline > now {
        sleep_high_res(*deadline - now);
    } else if now.duration_since(*deadline) > Duration::from_millis(20) {
        *deadline = now;
    }
}
