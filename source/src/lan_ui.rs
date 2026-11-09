use super::*;

impl RinkaSoundBridgeApp {
    pub(super) fn stop_lan(&mut self) {
        self.lan_session.take();
        self.pipeline_a.stop_lan();
        self.pipeline_b.stop_lan();
    }

    fn change_role(&mut self, role: LanRole) {
        if role == self.lan_config.role {
            return;
        }
        self.save_config_now();
        self.stop_lan();
        if self.gpu_boost_lock_enabled {
            reset_gpu_boost_clock_lock_on_exit();
            self.gpu_boost_lock_enabled = false;
        }
        for pipeline in [&self.pipeline_a, &self.pipeline_b] {
            for slot in &pipeline.vst_slots {
                slot.shutdown_and_release();
            }
        }
        self.pipeline_a = AudioPipeline::new("Line A");
        self.pipeline_b = AudioPipeline::new("Line B");
        self.lan_config.role = role;
        self.saved_config.lan = self.lan_config.clone();
        self.lan_error.clear();
        self.vst_picker.open = false;
        self.asio_setting_open = false;
        self.dsp_rsb_setting_open = false;
        self.rsb_asio_setting_open = false;
        if role == LanRole::Client {
            self.scanned_vst_cache.clear();
        } else {
            if self.scanned_vst_cache.is_empty() {
                self.scanned_vst_cache = VSTScanner::scan_plugins();
            }
            self.asio_notes_text = load_or_create_asio_notes_txt();
            if role == LanRole::Local {
                ensure_asio_requirements_installed(false);
            }
        }
        self.load_config_on_startup();
        self.save_config_now();
    }

    pub(super) fn render_mode_selector(&mut self, ui: &mut egui::Ui) {
        let mut role = self.lan_config.role;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Rinka SoundBridge").strong());
            ui.add_space(16.0);
            ui.selectable_value(&mut role, LanRole::Local, "Local");
            ui.selectable_value(&mut role, LanRole::Client, "LAN Client · PC 1");
            ui.selectable_value(&mut role, LanRole::Server, "LAN Server · PC 2");
        });
        self.change_role(role);
    }

    fn start_lan(&mut self) {
        self.lan_error.clear();
        self.asio_setting_open = false;
        self.dsp_rsb_setting_open = false;
        self.rsb_asio_setting_open = false;
        let result = if self.lan_config.role == LanRole::Client {
            LanSession::start_client(&self.lan_config)
        } else {
            self.pipeline_a.sample_rate = 48000;
            self.pipeline_b.sample_rate = 48000;
            self.dsp_rsb_config.sync_sample_rate_with_rsb = true;
            self.dsp_rsb_config.manual_sample_rate = 48000;
            self.pipeline_a.dsp_rsb_config = self.dsp_rsb_config.clone();
            self.pipeline_b.dsp_rsb_config = self.dsp_rsb_config.clone();
            self.pipeline_a.rsb_asio_config = self.rsb_asio_config.clone();
            self.pipeline_b.rsb_asio_config = self.rsb_asio_config.clone();
            if let Err(error) = self.lan_config.validate() {
                self.lan_error = error;
                return;
            }
            ensure_asio_requirements_installed(false);
            if let Err(error) = self.sync_unified_asio_config(true) {
                self.lan_error = error;
                return;
            }
            LanSession::start_server(&self.lan_config, &mut self.pipeline_a, &mut self.pipeline_b)
        };
        match result {
            Ok(session) => {
                self.lan_session = Some(session);
                self.trigger_auto_save();
            }
            Err(error) => {
                self.stop_lan();
                self.lan_error = error;
            }
        }
    }

    pub(super) fn render_lan_controls(&mut self, ui: &mut egui::Ui) {
        let server = self.lan_config.role == LanRole::Server;
        let active = self.lan_session.is_some();
        let before = self.lan_config.clone();
        ui.heading(if server {
            "LAN Server · PC 2"
        } else {
            "LAN Client · PC 1"
        });
        ui.label(if server {
            "LAN microphone → Line A → Vonovox → Line B → LAN return"
        } else {
            "Microphone → PC 2 processing → virtual audio cable"
        });
        ui.add_space(8.0);
        ui.add_enabled_ui(!active, |ui| {
            egui::Grid::new("lan_connection").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                ui.label(if server { "Listen address" } else { "Server IPv4" });
                if server {
                    ui.label(format!("{} (listens on all IPv4 adapters)", self.lan_address));
                } else { ui.text_edit_singleline(&mut self.lan_config.server_ip); }
                ui.end_row();
                ui.label("UDP port");
                ui.add(egui::DragValue::new(&mut self.lan_config.port).clamp_range(1..=65535));
                ui.end_row();
                ui.label("Receive buffer");
                ui.add(egui::Slider::new(&mut self.lan_config.receive_buffer_ms, 2.5..=30.0).step_by(2.5).suffix(" ms"));
                ui.end_row();
                ui.label("Session format"); ui.label("48 kHz · float32 PCM · mono mic / stereo return"); ui.end_row();
                if server {
                    ui.label("Vonovox ASIO bridge");
                    egui::ComboBox::from_id_source("lan_bridge").selected_text(match self.lan_config.bridge {
                        ServerBridge::RsbAsio => "RSB ASIO", ServerBridge::SoundBridgeAsio => "Rinka SoundBridge ASIO",
                    }).show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.lan_config.bridge, ServerBridge::SoundBridgeAsio, "Rinka SoundBridge ASIO");
                        ui.selectable_value(&mut self.lan_config.bridge, ServerBridge::RsbAsio, "RSB ASIO");
                    });
                    ui.end_row();
                }
            });
            if !server {
                ui.separator();
                egui::Grid::new("lan_devices").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    ui.label("Microphone method");
                    egui::ComboBox::from_id_source("lan_capture_method").selected_text(match self.lan_config.capture_backend {
                        CaptureBackend::WasapiShared => "WASAPI Shared", CaptureBackend::WasapiExclusive => "WASAPI Exclusive",
                        CaptureBackend::Asio => "ASIO Native",
                    }).show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.lan_config.capture_backend, CaptureBackend::WasapiShared, "WASAPI Shared");
                        ui.selectable_value(&mut self.lan_config.capture_backend, CaptureBackend::WasapiExclusive, "WASAPI Exclusive");
                        ui.selectable_value(&mut self.lan_config.capture_backend, CaptureBackend::Asio, "ASIO Native");
                    }); ui.end_row();
                    ui.label("Microphone device");
                    if self.lan_config.capture_backend == CaptureBackend::Asio {
                        device_picker(ui, "lan_asio", &mut self.lan_config.asio_driver, self.asio_drivers.iter().map(|d| d.name.as_str()));
                    } else {
                        device_picker(ui, "lan_microphone", &mut self.lan_config.input_device, self.wasapi_inputs.iter().map(|d| d.name.as_str()));
                    }
                    ui.end_row();
                    ui.label("Microphone channel");
                    let mut channel = self.lan_config.input_channel + 1;
                    if ui.add(egui::DragValue::new(&mut channel).clamp_range(1..=64)).changed() { self.lan_config.input_channel = channel - 1; }
                    ui.end_row();
                    ui.label("Return playback device");
                    device_picker(ui, "lan_return", &mut self.lan_config.return_device, self.wasapi_outputs.iter().map(|d| d.name.as_str()));
                    ui.end_row();
                });
                ui.small("Choose the virtual cable's playback endpoint here; other apps use its recording endpoint.");
                ui.collapsing("Device buffer and latency", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Buffer samples");
                        egui::ComboBox::from_id_source("lan_device_buffer").selected_text(self.lan_config.buffer_size.to_string()).show_ui(ui, |ui| {
                            for size in [32, 64, 128, 256, 512, 1024, 2048, 4096] { ui.selectable_value(&mut self.lan_config.buffer_size, size, size.to_string()); }
                        });
                        ui.label("Mic latency"); ui.add(egui::DragValue::new(&mut self.lan_config.input_latency_ms).clamp_range(0.0..=500.0).suffix(" ms"));
                        ui.label("Return latency"); ui.add(egui::DragValue::new(&mut self.lan_config.return_latency_ms).clamp_range(0.0..=500.0).suffix(" ms"));
                    });
                    if self.lan_config.capture_backend == CaptureBackend::Asio && ui.button("Open ASIO control panel").clicked() {
                        if let Err(error) = open_asio_control_panel(&self.lan_config.asio_driver) { self.lan_error = error; }
                    }
                    ui.small("WASAPI Exclusive retains the existing shared-mode fallback when a device rejects exclusive access.");
                });
            }
        });
        if !server {
            ui.separator();
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.lan_config.mic_muted, "Mute mic");
                ui.add(
                    egui::Slider::new(&mut self.lan_config.mic_gain, 0.0..=2.0).text("Mic level"),
                );
                ui.checkbox(&mut self.lan_config.return_muted, "Mute return");
                ui.add(
                    egui::Slider::new(&mut self.lan_config.return_gain, 0.0..=2.0)
                        .text("Return level"),
                );
            });
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let label = match (server, active) {
                (true, true) => "Stop Server",
                (true, false) => "Start Server",
                (false, true) => "Disconnect",
                (false, false) => "Connect",
            };
            if ui.button(label).clicked() {
                if active {
                    self.stop_lan();
                } else {
                    self.start_lan();
                }
            }
            if ui
                .add_enabled(!active, egui::Button::new("Refresh devices"))
                .clicked()
            {
                self.refresh_audio_devices();
            }
            if ui.button("Run in system tray").clicked() {
                self.save_config_now();
                self.tray_manager.minimize_to_tray(APP_WINDOW_TITLE);
            }
            if server
                && ui
                    .add_enabled(!active, egui::Button::new("ASIO bridge settings"))
                    .clicked()
            {
                if self.lan_config.bridge == ServerBridge::RsbAsio {
                    self.rsb_asio_setting_open = true;
                } else {
                    self.dsp_rsb_setting_open = true;
                }
            }
        });
        if let Some(session) = &self.lan_session {
            let s = &session.stats;
            s.update_controls(&self.lan_config);
            let color = if s.faulted.load(Ordering::Relaxed) {
                Palette::ACCENT_CORAL
            } else if s.connected.load(Ordering::Relaxed) {
                Palette::ACCENT_GREEN
            } else {
                Palette::TEXT_SECONDARY
            };
            ui.colored_label(color, s.message.lock().as_str());
            ui.label(format!("RTT {:.2} ms  ·  Receive fill {:.1} ms  ·  Lost {}  ·  Late {}  ·  Underruns {}  ·  Overruns {}  ·  Drift {:+} ppm",
                s.rtt_us.load(Ordering::Relaxed) as f64 / 1000.0, s.fill_frames.load(Ordering::Relaxed) as f64 / 48.0,
                s.lost.load(Ordering::Relaxed), s.late.load(Ordering::Relaxed), s.underruns.load(Ordering::Relaxed),
                s.overruns.load(Ordering::Relaxed), s.drift_ppm.load(Ordering::Relaxed)));
            if !server {
                ui.label(format!(
                    "Mic peak {:.0}%  ·  Return peak {:.0}%",
                    f32::from_bits(s.mic_peak.load(Ordering::Relaxed)) * 100.0,
                    f32::from_bits(s.return_peak.load(Ordering::Relaxed)) * 100.0
                ));
            }
        } else {
            ui.label(if server { "Start the server before starting Vonovox's audio stream. Select the matching ASIO driver at 48000 Hz." } else { "Disconnected" });
        }
        if !self.lan_error.is_empty() {
            ui.colored_label(Palette::ACCENT_CORAL, &self.lan_error);
        }
        if self.lan_config != before {
            if server && !active {
                let backend = if self.lan_config.bridge == ServerBridge::RsbAsio {
                    DriverBackend::RsbAsio
                } else {
                    DriverBackend::AsioDspRsb
                };
                self.pipeline_a.driver_backend = backend;
                self.pipeline_b.driver_backend = backend;
            }
            self.trigger_auto_save();
        }
    }
}

fn device_picker<'a>(
    ui: &mut egui::Ui,
    id: &str,
    selected: &mut String,
    names: impl Iterator<Item = &'a str>,
) {
    egui::ComboBox::from_id_source(id)
        .width(380.0)
        .selected_text(if selected.is_empty() {
            "Select device"
        } else {
            selected.as_str()
        })
        .show_ui(ui, |ui| {
            for name in names {
                ui.selectable_value(selected, name.to_string(), name);
            }
        });
}

pub(super) fn local_address() -> String {
    // UDP connect selects the local route without transmitting a datagram.
    (|| {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("192.0.2.1:9").ok()?;
        Some(socket.local_addr().ok()?.ip().to_string())
    })()
    .unwrap_or_else(|| "Use ipconfig to find the Ethernet IPv4 address".into())
}
