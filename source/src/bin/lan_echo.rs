use rinka_soundbridge_core::lan::diagnostics;
use std::time::Duration;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("server") => {
            let port = args.get(2).map_or(Ok(40100), |v| v.parse::<u16>()).map_err(|e| e.to_string())?;
            let seconds = args.get(3).map_or(Ok(3600), |v| v.parse::<u64>()).map_err(|e| e.to_string())?.clamp(1, 86400);
            println!("LAN echo server on UDP {port} for {seconds} seconds (no audio devices or Vonovox)");
            diagnostics::echo_server(port, Duration::from_secs(seconds))
        }
        Some("probe") if args.len() >= 3 => {
            let port = args.get(3).map_or(Ok(40100), |v| v.parse::<u16>()).map_err(|e| e.to_string())?;
            let seconds = args.get(4).map_or(Ok(15), |v| v.parse::<u64>()).map_err(|e| e.to_string())?.clamp(3, 86400);
            let report = diagnostics::probe(&args[2], port, Duration::from_secs(seconds))?;
            println!("{} echoes | median {:.2} ms | p95 {:.2} ms | lost {} | underruns {}",
                report.measurements, report.median_ms, report.p95_ms, report.lost, report.underruns);
            Ok(())
        }
        _ => Err("Usage: lan_echo server [port] [seconds]\n       lan_echo probe SERVER_IPV4 [port] [seconds]".into()),
    }
}
