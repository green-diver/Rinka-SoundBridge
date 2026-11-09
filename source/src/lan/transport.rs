use super::{
    protocol::{Kind, Packet, MAX_DATAGRAM},
    LanConfig, LanRole, LanStats,
};
use rtrb::{Consumer, Producer, RingBuffer};
use std::{
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(3);
const QUEUE_PACKETS: usize = 40; // 100 ms, bounded even if an endpoint stalls.

pub(crate) struct AudioIo {
    pub tx: Producer<Packet>,
    pub rx: Consumer<Packet>,
}
pub(crate) struct Transport {
    pub running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Transport {
    pub fn start(config: &LanConfig, stats: Arc<LanStats>) -> Result<(Self, AudioIo), String> {
        let server = config.role == LanRole::Server;
        let address: SocketAddr = if server {
            (Ipv4Addr::UNSPECIFIED, config.port).into()
        } else {
            (
                config
                    .server_ip
                    .parse::<Ipv4Addr>()
                    .map_err(|_| "Invalid server IPv4 address")?,
                config.port,
            )
                .into()
        };
        let socket = UdpSocket::bind(if server {
            address
        } else {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        })
        .map_err(|e| format!("Cannot bind UDP socket: {e}"))?;
        socket.set_nonblocking(true).map_err(|e| e.to_string())?;
        let (audio_tx, network_tx) = RingBuffer::new(QUEUE_PACKETS);
        let (network_rx, audio_rx) = RingBuffer::new(QUEUE_PACKETS);
        let running = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&running);
        let worker = thread::Builder::new()
            .name("rinka-lan-udp".into())
            .spawn(move || run(socket, server, address, stats, flag, network_tx, network_rx))
            .map_err(|e| e.to_string())?;
        Ok((
            Self {
                running,
                worker: Some(worker),
            },
            AudioIo {
                tx: audio_tx,
                rx: audio_rx,
            },
        ))
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn random_id() -> u64 {
    rand::random::<u64>().max(1)
}

fn send(
    socket: &UdpSocket,
    peer: SocketAddr,
    packet: &Packet,
    buffer: &mut [u8; MAX_DATAGRAM],
    stats: &LanStats,
) -> bool {
    let size = packet.encode(buffer);
    match socket.send_to(&buffer[..size], peer) {
        Ok(_) => true,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            false
        }
        Err(error) => {
            stats.fail(format!("UDP send failed: {error}"));
            false
        }
    }
}

fn run(
    socket: UdpSocket,
    server: bool,
    address: SocketAddr,
    stats: Arc<LanStats>,
    running: Arc<AtomicBool>,
    mut outgoing: Consumer<Packet>,
    mut incoming: Producer<Packet>,
) {
    let mut peer = if server { None } else { Some(address) };
    let mut session = 0;
    let mut nonce = random_id();
    let mut peer_nonce = 0;
    let mut last_rx = Instant::now();
    let mut last_hello = Instant::now() - Duration::from_secs(1);
    let mut last_ping = Instant::now();
    let mut ping: Option<(u64, Instant)> = None;
    let origin = Instant::now();
    // An extra byte detects truncated/oversized datagrams instead of decoding a valid prefix.
    let mut receive = [0u8; MAX_DATAGRAM + 1];
    let mut transmit = [0u8; MAX_DATAGRAM];
    stats.status(if server {
        format!("Listening on UDP {}", address.port())
    } else {
        format!("Connecting to {address}")
    });
    while running.load(Ordering::Acquire) && !stats.faulted.load(Ordering::Acquire) {
        if server {
            crate::asio_host::mark_lan_server_active(true);
        }
        let now = Instant::now();
        if session != 0 && now.duration_since(last_rx) >= TIMEOUT {
            session = 0;
            peer = if server { None } else { Some(address) };
            nonce = random_id();
            ping = None;
            stats.new_generation(false);
            stats.status(if server {
                "Client timed out; listening"
            } else {
                "Server timed out; reconnecting"
            });
        }
        if !server && session == 0 && now.duration_since(last_hello) >= Duration::from_millis(500) {
            send(
                &socket,
                address,
                &Packet::control(Kind::Hello, nonce, 0),
                &mut transmit,
                &stats,
            );
            last_hello = now;
        }
        for _ in 0..64 {
            let (size, source) = match socket.recv_from(&mut receive) {
                Ok(value) => value,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                // Windows reports ICMP port-unreachable as a UDP receive reset.
                // A server may be down while a client is trying to reconnect.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    continue
                }
                Err(e) if e.raw_os_error() == Some(10040) => {
                    stats.invalid.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Err(e) => {
                    stats.fail(format!("UDP receive failed: {e}"));
                    break;
                }
            };
            let mut packet = match Packet::decode(&receive[..size]) {
                Ok(packet) => packet,
                Err(error) => {
                    stats.invalid.fetch_add(1, Ordering::Relaxed);
                    if !server && source == address && error == "Incompatible LAN protocol version"
                    {
                        stats.fail(error);
                    }
                    continue;
                }
            };
            if server && packet.kind == Kind::Hello && packet.session != 0 {
                if session != 0 && peer != Some(source) {
                    send(
                        &socket,
                        source,
                        &Packet::control(Kind::Busy, packet.session, 0),
                        &mut transmit,
                        &stats,
                    );
                    continue;
                }
                if session == 0 || peer_nonce != packet.session {
                    peer = Some(source);
                    peer_nonce = packet.session;
                    session = random_id();
                    stats.new_generation(true);
                    stats.status(format!("Connected to {source}"));
                }
                last_rx = now;
                send(
                    &socket,
                    source,
                    &Packet::control(Kind::Welcome, session, peer_nonce),
                    &mut transmit,
                    &stats,
                );
                continue;
            }
            if !server && source == address {
                if packet.kind == Kind::Busy && packet.session == nonce {
                    stats.status("Server is busy with another client; retrying");
                    continue;
                }
                if packet.kind == Kind::Welcome && packet.position == nonce && packet.session != 0 {
                    if session != packet.session {
                        session = packet.session;
                        stats.new_generation(true);
                        stats.status(format!("Connected to {source}"));
                    }
                    last_rx = now;
                    continue;
                }
            }
            if session == 0 || peer != Some(source) || packet.session != session {
                continue;
            }
            match packet.kind {
                Kind::Audio if packet.direction == if server { 1 } else { 2 } => {
                    last_rx = now;
                    packet.generation = stats.generation.load(Ordering::Acquire);
                    if incoming.push(packet).is_err() {
                        stats.overruns.fetch_add(1, Ordering::Relaxed);
                    }
                    stats.received.fetch_add(1, Ordering::Relaxed);
                }
                Kind::Ping => {
                    last_rx = now;
                    send(
                        &socket,
                        source,
                        &Packet::control(Kind::Pong, session, packet.position),
                        &mut transmit,
                        &stats,
                    );
                }
                Kind::Pong => {
                    if let Some((token, sent)) = ping {
                        if packet.position == token {
                            last_rx = now;
                            stats
                                .rtt_us
                                .store(sent.elapsed().as_micros() as u64, Ordering::Relaxed);
                            ping = None;
                        }
                    }
                }
                Kind::Bye => {
                    session = 0;
                    peer = if server { None } else { Some(address) };
                    nonce = random_id();
                    ping = None;
                    stats.new_generation(false);
                    stats.status(if server {
                        "Client disconnected; listening"
                    } else {
                        "Server stopped; reconnecting"
                    });
                }
                _ => {
                    stats.invalid.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let generation = stats.generation.load(Ordering::Acquire);
        for _ in 0..QUEUE_PACKETS {
            let Ok(mut packet) = outgoing.pop() else {
                break;
            };
            if session == 0 || packet.generation != generation {
                continue;
            }
            packet.session = session;
            if send(&socket, peer.unwrap(), &packet, &mut transmit, &stats) {
                stats.sent.fetch_add(1, Ordering::Relaxed);
            } else {
                stats.overruns.fetch_add(1, Ordering::Relaxed);
            }
        }
        if session != 0 && now.duration_since(last_ping) >= Duration::from_secs(1) {
            let token = origin.elapsed().as_micros() as u64 + 1;
            send(
                &socket,
                peer.unwrap(),
                &Packet::control(Kind::Ping, session, token),
                &mut transmit,
                &stats,
            );
            ping = Some((token, Instant::now()));
            last_ping = now;
        }
        crate::win32::sleep_high_res(Duration::from_micros(500));
    }
    if session != 0 {
        send(
            &socket,
            peer.unwrap(),
            &Packet::control(Kind::Bye, session, 0),
            &mut transmit,
            &stats,
        );
    }
    stats.new_generation(false);
    if server {
        crate::asio_host::mark_lan_server_active(false);
    }
}
