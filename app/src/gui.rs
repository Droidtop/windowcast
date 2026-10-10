//! The role windows, drawn with egui (eframe over OpenGL): a host window
//! and a client window, each its own native window. A program running
//! both roles shows both windows; streamed windows open in windows of
//! their own (client-windows), never inside these.

use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, RichText};
use windowcast_protocol::{BackendKind, ContentHint, VideoCodec};

use crate::client::{ClientRole, ClientSnapshot};
use crate::config::{HostSettings, Roles};
use crate::host::{HostRole, HostSnapshot};
use crate::terminal::SshRequest;

type Role<T> = Option<Result<Arc<T>, String>>;

pub fn run(roles: Roles, host: Role<HostRole>, client: Role<ClientRole>) -> Result<(), String> {
    let title = match roles {
        Roles::Client => "windowcast client",
        _ => "windowcast host",
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([980.0, 720.0]),
        ..Default::default()
    };
    let app = App {
        roles,
        host,
        client,
        address: "127.0.0.1:47100".into(),
        pin: String::new(),
        username: String::new(),
        password: String::new(),
        new_account: Default::default(),
        clipboard: String::new(),
        ssh: SshRequest::default(),
        launch_line: String::new(),
        windows_password: String::new(),
        show_client: true,
        error: None,
    };
    eframe::run_native(title, options, Box::new(|_| Ok(Box::new(app)))).map_err(|e| e.to_string())
}

struct App {
    roles: Roles,
    host: Role<HostRole>,
    client: Role<ClientRole>,
    address: String,
    pin: String,
    /// Signing in with an account on the client.
    username: String,
    password: String,
    /// A local account being made on the host: name, password, groups
    /// (comma-separated).
    new_account: (String, String, String),
    clipboard: String,
    ssh: SshRequest,
    launch_line: String,
    /// The Windows password a RemoteApp launch asked for; cleared once used.
    windows_password: String,
    /// The client window, when both roles run (closing it hides it).
    show_client: bool,
    error: Option<String>,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(100));
        self.terminal_windows(ctx);
        match self.roles {
            Roles::Client => {
                egui::CentralPanel::default().show(ctx, |ui| self.client_ui(ui));
            }
            Roles::Host => {
                egui::CentralPanel::default().show(ctx, |ui| self.host_ui(ui));
            }
            Roles::Both => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if !self.show_client && ui.button("Open the client window").clicked() {
                        self.show_client = true;
                    }
                    self.host_ui(ui);
                });
                if self.show_client {
                    ctx.show_viewport_immediate(
                        egui::ViewportId::from_hash_of("windowcast client"),
                        egui::ViewportBuilder::default()
                            .with_title("windowcast client")
                            .with_inner_size([980.0, 720.0]),
                        |ctx, _| {
                            egui::CentralPanel::default().show(ctx, |ui| self.client_ui(ui));
                            if ctx.input(|i| i.viewport().close_requested()) {
                                self.show_client = false;
                            }
                        },
                    );
                }
            }
        }
    }
}

fn backend_name(backend: BackendKind) -> &'static str {
    match backend {
        BackendKind::Native => "Native",
        BackendKind::Passthrough => "Passthrough",
        BackendKind::Desktop => "Desktop",
        BackendKind::GameStream => "GameStream",
        BackendKind::Rdp => "RDP",
        BackendKind::Vnc => "VNC",
        BackendKind::Other => "Other",
    }
}

fn codec_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "H.264",
        VideoCodec::H265 => "H.265",
        VideoCodec::Av1 => "AV1",
    }
}

fn content_name(content: ContentHint) -> &'static str {
    match content {
        ContentHint::General => "general",
        ContentHint::Text => "text",
        ContentHint::Game => "game",
        ContentHint::Video => "video",
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

fn clip(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        text.to_owned()
    } else {
        format!("{}...", text.chars().take(chars).collect::<String>())
    }
}

/// The backends a client may ask for, and whether they are built.
const BACKENDS: [(BackendKind, &str, bool); 6] = [
    (BackendKind::Native, "Native", true),
    (
        BackendKind::Passthrough,
        "Passthrough (not built: host serves native)",
        true,
    ),
    (
        BackendKind::Desktop,
        "Desktop (cut from the whole screen)",
        true,
    ),
    (BackendKind::GameStream, "GameStream (not built yet)", false),
    (
        BackendKind::Rdp,
        "RDP (sharp text; on the same network)",
        true,
    ),
    (BackendKind::Vnc, "VNC (not built yet)", false),
];

impl App {
    /// One window per open terminal.
    fn terminal_windows(&mut self, ctx: &egui::Context) {
        let Some(Ok(client)) = &self.client else {
            return;
        };
        for entry in client.terminals().open() {
            let id = egui::ViewportId::from_hash_of(("windowcast terminal", entry.id));
            let builder = egui::ViewportBuilder::default()
                .with_title(format!("windowcast terminal: {}", entry.title))
                .with_inner_size([900.0, 560.0]);
            ctx.show_viewport_immediate(id, builder, |ctx, _| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| crate::terminal::terminal_ui(ui, &entry));
                if ctx.input(|i| i.viewport().close_requested()) {
                    client.terminals().close(entry.id);
                }
            });
        }
    }

    /// Shells and application launches: on the connected host, and on any
    /// SSH server.
    fn terminals_ui(&mut self, ui: &mut egui::Ui, client: &Arc<ClientRole>, connected: bool) {
        ui.heading("Terminals and applications");
        ui.horizontal(|ui| {
            ui.add_enabled_ui(connected, |ui| {
                if ui.button("Open a shell on the host").clicked() {
                    client.open_host_terminal_in_background();
                }
                ui.label("Start on the host");
                ui.add(
                    egui::TextEdit::singleline(&mut self.launch_line)
                        .desired_width(200.0)
                        .hint_text("program and arguments"),
                );
                if ui.button("Start").clicked() && !self.launch_line.trim().is_empty() {
                    client.launch_in_background(self.launch_line.trim().to_owned(), None);
                }
            });
        });
        let progress = client.terminals().progress();
        if let Some(user) = &progress.password_for {
            // A RemoteApp of the host's Remote Desktop, which needs the
            // user's own Windows password.
            ui.horizontal(|ui| {
                ui.label(format!("Windows password for {user}"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.windows_password)
                        .password(true)
                        .desired_width(160.0),
                );
                if ui.button("Start").clicked() && !self.windows_password.is_empty() {
                    let password = std::mem::take(&mut self.windows_password);
                    client.launch_in_background(self.launch_line.trim().to_owned(), Some(password));
                }
            });
        }
        if let Some(done) = &progress.launched {
            ui.label(RichText::new(done).weak());
        }
        ui.horizontal(|ui| {
            ui.label("SSH");
            ui.add(
                egui::TextEdit::singleline(&mut self.ssh.user)
                    .desired_width(80.0)
                    .hint_text("user"),
            );
            ui.label("@");
            ui.add(
                egui::TextEdit::singleline(&mut self.ssh.host)
                    .desired_width(140.0)
                    .hint_text("server"),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.ssh.port)
                    .desired_width(40.0)
                    .hint_text("22"),
            );
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.ssh.password)
                    .password(true)
                    .desired_width(120.0)
                    .hint_text("password"),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.ssh.key_file)
                    .desired_width(160.0)
                    .hint_text("or key file"),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.ssh.passphrase)
                    .password(true)
                    .desired_width(100.0)
                    .hint_text("key passphrase"),
            );
            ui.add_enabled_ui(!progress.busy, |ui| {
                if ui.button("Log in").clicked() {
                    client.open_ssh_in_background(self.ssh.clone(), None);
                }
            });
            if progress.busy {
                ui.label("Connecting...");
            }
        });
        if let Some((server, fingerprint)) = &progress.untrusted {
            ui.horizontal(|ui| {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("{server} is not known yet. Its key is {fingerprint}."),
                );
                if ui.button("Trust it and log in").clicked() {
                    client.open_ssh_in_background(self.ssh.clone(), Some(fingerprint.clone()));
                }
            });
        }
        if let Some(error) = &progress.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        for entry in client.terminals().open() {
            ui.label(RichText::new(format!("open: {}", entry.title)).weak());
        }
    }

    fn host_ui(&mut self, ui: &mut egui::Ui) {
        let host = match &self.host {
            Some(Ok(host)) => Arc::clone(host),
            Some(Err(e)) => {
                ui.heading("Host");
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
            None => return,
        };
        let snapshot: HostSnapshot = host.snapshot();
        let mut settings = host.settings();
        let before = settings.clone();
        egui::ScrollArea::vertical().id_salt("host").show(ui, |ui| {
            ui.heading("Pairing");
            match &snapshot.pin {
                Some(pin) => {
                    ui.label(RichText::new(pin).size(30.0).monospace().strong());
                    ui.label("Enter this PIN on the client to pair.");
                }
                None => {
                    ui.label(RichText::new("Pairing closed").size(20.0).weak());
                }
            }
            ui.horizontal(|ui| {
                let open = if snapshot.pin.is_some() { "New PIN" } else { "Open pairing" };
                if ui.button(open).clicked() {
                    host.pairing(true);
                }
                if ui.button("Close pairing").clicked() {
                    host.pairing(false);
                }
            });
            ui.label(
                RichText::new(format!(
                    "{} host on {} - identity {}",
                    crate::platform::NAME,
                    host.listen,
                    windowcast_client::fingerprint(&snapshot.identity)
                ))
                .weak(),
            );
            ui.separator();

            ui.heading("Encoding");
            encoding_ui(ui, &host, &snapshot, &mut settings);
            ui.horizontal(|ui| {
                ui.checkbox(
                    &mut settings.input,
                    "Clients may drive streamed windows and use gamepads",
                );
                ui.checkbox(&mut settings.clipboard, "Share the clipboard");
                ui.checkbox(
                    &mut settings.microphone,
                    "Clients may use their microphone here",
                );
                ui.checkbox(&mut settings.rdp, "Offer windows over RDP");
            });
            remote_apps_ui(ui, &snapshot, &mut settings.remote_apps);
            ui.horizontal(|ui| {
                ui.checkbox(
                    &mut settings.away,
                    "Reachable away from home by trusted clients",
                );
                ui.label("on UDP");
                ui.add(egui::DragValue::new(&mut settings.away_port).range(1024..=65535));
            });
            if let Some(away) = &snapshot.away {
                ui.label(
                    RichText::new(format!(
                        "Away: {}; {}; {} trusted client(s) found",
                        away.mapped
                            .map(|m| format!("the router gives {m}"))
                            .unwrap_or_else(|| "asking STUN for this network's address".into()),
                        away.announced.as_deref().unwrap_or("not announced yet"),
                        away.found.len()
                    ))
                    .weak(),
                );
            }
            ui.separator();

            ui.heading("Live streams");
            if snapshot.streams.is_empty() {
                ui.label(RichText::new("Nothing is streaming.").weak());
            } else {
                egui::Grid::new("host-streams")
                    .striped(true)
                    .show(ui, |ui| {
                        for heading in [
                            "Window",
                            "Client",
                            "Backend",
                            "Codec",
                            "Encoder",
                            "Size",
                            "fps",
                            "Mbit/s",
                            "Keyframes",
                            "Sound",
                            "Network",
                            "Time",
                        ] {
                            ui.strong(heading);
                        }
                        ui.end_row();
                        for stream in &snapshot.streams {
                            ui.label(clip(&stream.title, 40));
                            ui.label(short(&stream.client));
                            if stream.requested == stream.backend {
                                ui.label(backend_name(stream.backend));
                            } else {
                                ui.label(format!(
                                    "{} -> {} (fallback)",
                                    backend_name(stream.requested),
                                    backend_name(stream.backend)
                                ));
                            }
                            ui.label(codec_name(stream.codec));
                            ui.label(clip(&stream.encoder, 48));
                            ui.label(
                                stream
                                    .size
                                    .map(|(w, h)| format!("{w}x{h}"))
                                    .unwrap_or_default(),
                            );
                            ui.label(format!("{:.1}", stream.fps));
                            ui.label(format!("{:.2}", stream.mbps));
                            ui.label(format!(
                                "{} ({} asked)",
                                stream.keyframes, stream.keyframe_requests
                            ));
                            ui.label(match stream.audio_packets {
                                Some(packets) => format!("{packets} packets"),
                                None => "none".into(),
                            });
                            ui.label(network(&stream.quality));
                            ui.label(format!("{} s", stream.seconds));
                            ui.end_row();
                        }
                    });
            }
            ui.separator();

            ui.heading("Windows on this host");
            egui::Grid::new("host-windows")
                .striped(true)
                .show(ui, |ui| {
                    for heading in ["Window", "App", "Detected", "Rules ask for", "Host serves"] {
                        ui.strong(heading);
                    }
                    ui.end_row();
                    for window in &snapshot.windows {
                        ui.label(clip(&window.info.title, 50));
                        ui.label(&window.info.app_id);
                        ui.label(content_name(window.info.content));
                        ui.label(backend_name(window.default_backend));
                        if window.served == window.default_backend {
                            ui.label(backend_name(window.served));
                        } else {
                            ui.label(format!("{} (fallback)", backend_name(window.served)));
                        }
                        ui.end_row();
                    }
                });
            ui.separator();

            ui.heading("Clients");
            if snapshot.clients.is_empty() && snapshot.trusted.is_empty() {
                ui.label(RichText::new("No client has paired yet.").weak());
            }
            for client in &snapshot.clients {
                ui.label(format!(
                    "{} connected from {} for {} s{}",
                    short(&client.peer),
                    client.address,
                    client.seconds,
                    client
                        .account
                        .as_ref()
                        .map(|a| format!(", signed in as {a}"))
                        .unwrap_or_default()
                ));
            }
            for peer in &snapshot.trusted {
                ui.horizontal(|ui| {
                    ui.label(format!("trusted {}", short(peer)));
                    if ui.button("Forget").clicked() {
                        if let Err(e) = host.forget(peer) {
                            self.error = Some(e);
                        }
                    }
                });
            }
            ui.separator();

            ui.heading("Accounts");
            let mut sign_in = settings.accounts.is_some();
            if ui
                .checkbox(&mut sign_in, "Clients may sign in with an account")
                .changed()
            {
                settings.accounts = sign_in.then(windowcast_accounts::Config::default);
            }
            ui.label(
                RichText::new(
                    "Identity providers (OpenID Connect), LDAP, Kerberos and the access policy                      are set in config.json under host.accounts (docs/ACCOUNTS.md).",
                )
                .weak(),
            );
            if let Some(accounts) = &snapshot.accounts {
                let (local, registered) = (&accounts.local, &accounts.registered);
                for (name, groups) in local {
                    ui.horizontal(|ui| {
                        ui.label(format!("{name} {}", groups.join(", ")));
                        if ui.button("Remove").clicked() {
                            if let Err(e) = host.remove_account(name) {
                                self.error = Some(e);
                            }
                        }
                    });
                }
                ui.horizontal(|ui| {
                    let (name, password, groups) = &mut self.new_account;
                    ui.add(
                        egui::TextEdit::singleline(name)
                            .desired_width(100.0)
                            .hint_text("name"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(password)
                            .password(true)
                            .desired_width(100.0)
                            .hint_text("password"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(groups)
                            .desired_width(140.0)
                            .hint_text("groups, comma-separated"),
                    );
                    if ui.button("Add account").clicked() {
                        let groups: Vec<String> = groups
                            .split(',')
                            .map(|g| g.trim().to_owned())
                            .filter(|g| !g.is_empty())
                            .collect();
                        match host.add_account(name, password, &groups) {
                            Ok(()) => self.new_account = Default::default(),
                            Err(e) => self.error = Some(e),
                        }
                    }
                });
                for device in registered {
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "{} signed in as {}, {} days left",
                            short(&device.peer),
                            device.account,
                            device.days_left
                        ));
                        if ui.button("Forget").clicked() {
                            if let Err(e) = host.unregister(&device.peer) {
                                self.error = Some(e);
                            }
                        }
                    });
                }
            }
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
        });
        if settings != before {
            if let Err(e) = host.apply(settings) {
                self.error = Some(e);
            }
        }
    }

    fn client_ui(&mut self, ui: &mut egui::Ui) {
        let client = match &self.client {
            Some(Ok(client)) => Arc::clone(client),
            Some(Err(e)) => {
                ui.heading("Client");
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
            None => return,
        };
        let snapshot: ClientSnapshot = client.snapshot();
        let config = client.store().get().client;
        egui::ScrollArea::vertical()
            .id_salt("client")
            .show(ui, |ui| {
                ui.heading("Connection");
                ui.label(
                    RichText::new(format!("This client: identity {}", short(&snapshot.identity)))
                        .weak(),
                );
                if let Some(address) = &snapshot.address {
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "Connected to {address} (host {}){}",
                            short(snapshot.host_id.as_deref().unwrap_or("")),
                            if snapshot.paired { ", paired just now" } else { "" }
                        ));
                        if let Some(rtt) = snapshot.rtt_ms {
                            ui.label(RichText::new(format!("round trip {rtt:.1} ms")).weak());
                        }
                        if ui.button("Disconnect").clicked() {
                            client.disconnect();
                        }
                    });
                } else if snapshot.connecting {
                    ui.label("Connecting...");
                } else {
                    ui.label(RichText::new("Not connected.").weak());
                }
                for saved in &config.saved {
                    ui.horizontal(|ui| {
                        ui.label(format!("{} - {}", saved.address, short(&saved.host_id)));
                        if ui.button("Connect").clicked() {
                            client.connect_in_background(saved.address.clone(), None);
                        }
                        if ui.button("Forget").clicked() {
                            let _ = client.forget(&saved.host_id);
                        }
                    });
                }
                ui.horizontal(|ui| {
                    ui.label("Host");
                    ui.add(egui::TextEdit::singleline(&mut self.address).desired_width(160.0));
                    ui.label("PIN");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.pin)
                            .desired_width(80.0)
                            .hint_text("first time"),
                    );
                    if ui.button("Connect").clicked() {
                        let pin = (!self.pin.trim().is_empty()).then(|| self.pin.trim().to_owned());
                        client.connect_in_background(self.address.trim().to_owned(), pin);
                        self.pin.clear();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Or sign in");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.username)
                            .desired_width(100.0)
                            .hint_text("user"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.password)
                            .password(true)
                            .desired_width(100.0)
                            .hint_text("password"),
                    );
                    if ui.button("Sign in").clicked() {
                        client.sign_in_in_background(
                            self.address.trim().to_owned(),
                            windowcast_client::SignIn::Password {
                                username: self.username.trim().to_owned(),
                                password: std::mem::take(&mut self.password),
                            },
                            None,
                        );
                    }
                    if ui.button("Other ways to sign in").clicked() {
                        client.ask_sign_in_options(self.address.trim().to_owned());
                    }
                });
                if let Some((address, options)) = &snapshot.sign_in_options {
                    ui.label(format!(
                        "{address}: host {}{}",
                        options.fingerprint,
                        if options.trusted {
                            ""
                        } else {
                            " (not trusted yet: check this matches the host's window)"
                        }
                    ));
                    let accept = (!options.trusted).then(|| options.host_id.clone());
                    ui.horizontal(|ui| {
                        for provider in &options.providers {
                            if ui.button(format!("Sign in with {}", provider.name)).clicked() {
                                client.sign_in_with_provider(
                                    address.clone(),
                                    provider.clone(),
                                    accept.clone(),
                                );
                            }
                        }
                        if options.kerberos_service.is_some()
                            && ui.button("Sign in with Kerberos").clicked()
                        {
                            client.sign_in_in_background(
                                address.clone(),
                                windowcast_client::SignIn::Kerberos,
                                accept.clone(),
                            );
                        }
                    });
                }
                if let Some(pending) = &snapshot.pending_trust {
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "{} is new to this client. Its identity is {}: sign in only if                              the host's window shows the same.",
                            pending.address, pending.fingerprint
                        ));
                        if ui.button("It matches: sign in").clicked() {
                            client.trust_and_sign_in(pending.clone());
                        }
                    });
                }
                if let Some(error) = &snapshot.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                ui.separator();

                ui.heading("Stream windows");
                stream_window_ui(ui, &client);
                ui.separator();

                self.terminals_ui(ui, &client, snapshot.address.is_some());
                ui.separator();

                ui.horizontal(|ui| {
                    ui.heading("Host windows");
                    if snapshot.address.is_some() && ui.button("Refresh").clicked() {
                        let _ = client.refresh();
                    }
                    let mut follow = client.follow_popups();
                    if ui
                        .checkbox(&mut follow, "Open a streamed window's dialogs, popups and menus")
                        .changed()
                    {
                        client.set_follow_popups(follow);
                    }
                });
                if snapshot.address.is_none() {
                    ui.label(RichText::new("Connect to a host to see its windows.").weak());
                }
                egui::Grid::new("client-windows").striped(true).show(ui, |ui| {
                    for heading in ["Window", "App", "Detected", "Backend", ""] {
                        ui.strong(heading);
                    }
                    ui.end_row();
                    for (depth, window) in grouped(&snapshot.windows) {
                        // A window another owns sits under it, with its kind.
                        let title = clip(&window.info.title, 50);
                        if depth == 0 {
                            ui.label(title);
                        } else {
                            ui.label(format!(
                                "{}- {title} ({})",
                                "    ".repeat(depth),
                                kind_name(window.info.kind)
                            ));
                        }
                        ui.label(&window.info.app_id);
                        ui.label(content_name(window.info.content));
                        let mut rule = window.rule;
                        egui::ComboBox::from_id_salt(("rule", window.info.id.0))
                            .selected_text(match rule {
                                Some(backend) => backend_name(backend).to_owned(),
                                None => format!("Rules: {}", backend_name(window.default_backend)),
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut rule,
                                    None,
                                    format!(
                                        "Rules: {} for {} content",
                                        backend_name(window.default_backend),
                                        content_name(window.info.content)
                                    ),
                                );
                                for (backend, label, built) in BACKENDS {
                                    ui.add_enabled_ui(built, |ui| {
                                        ui.selectable_value(&mut rule, Some(backend), label);
                                    });
                                }
                            });
                        if rule != window.rule {
                            client.set_rule(&window.info.app_id, rule);
                        }
                        ui.horizontal(|ui| {
                            if window.streaming {
                                if ui.button("Stop").clicked() {
                                    let _ = client.stop_stream(window.info.id.0);
                                }
                            } else if ui.button("Stream").clicked() {
                                let _ = client.start_stream(window.info.id.0);
                            }
                            if let Some(refused) = &window.refused {
                                ui.colored_label(ui.visuals().error_fg_color, refused);
                            }
                        });
                        ui.end_row();
                    }
                });
                ui.separator();

                ui.horizontal(|ui| {
                    ui.heading("Streams");
                    let mut auto = client.auto_switch();
                    if ui
                        .checkbox(&mut auto, "Switch between video and RDP by themselves")
                        .changed()
                    {
                        client.set_auto_switch(auto);
                    }
                });
                if snapshot.streams.is_empty() {
                    ui.label(RichText::new("Nothing is streaming.").weak());
                }
                for stream in &snapshot.streams {
                    let stats = &stream.stats;
                    ui.label(RichText::new(clip(&stream.title, 70)).strong());
                    let served = stream.backend.map_or("waiting", backend_name);
                    let asked = stream.requested.map_or("?", backend_name);
                    ui.label(format!(
                        "asked for {asked}, served {served}{} - {} - {}",
                        if stream.requested.is_some() && stream.requested != stream.backend {
                            " (fallback: not built yet)"
                        } else {
                            ""
                        },
                        stream.codec.map_or("", codec_name),
                        stats.decoder
                    ));
                    ui.label(format!(
                        "{} - {:.1} fps shown - {:.2} Mbit/s - keyframes {} - decode and present {:.1} ms{}",
                        stats
                            .size
                            .map(|(w, h)| format!("{w}x{h}"))
                            .unwrap_or_else(|| "no picture yet".into()),
                        stats.fps,
                        stats.mbps,
                        stats.keyframes,
                        stats.latency_ms,
                        if stats.resets > 0 {
                            format!(" - decoder resets {}", stats.resets)
                        } else {
                            String::new()
                        }
                    ));
                    if let Some(quality) = &stream.quality {
                        ui.label(format!("Sent at: {}", network(quality)));
                    }
                    // Changing the carrier keeps the window open.
                    ui.horizontal(|ui| {
                        ui.label("Switch to");
                        for (backend, label) in [
                            (windowcast_protocol::BackendKind::Native, "video"),
                            (windowcast_protocol::BackendKind::Rdp, "RDP pictures"),
                        ] {
                            let current = stream.backend == Some(backend);
                            if ui.add_enabled(!current, egui::Button::new(label)).clicked() {
                                if let Err(e) = client.switch_stream(stream.window, backend) {
                                    client.log(format!("no switch: {e}"));
                                }
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        let mut limits = stream.limits;
                        let mut changed = false;
                        ui.label("Limits (0: none):");
                        changed |= limit(ui, &mut limits.max_bitrate_kbps, "kbit/s", 50_000);
                        changed |= limit(ui, &mut limits.max_fps, "fps", 240);
                        changed |= limit(ui, &mut limits.max_height, "lines", 4320);
                        if changed {
                            client.set_limits(stream.window, limits);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(if stats.audio {
                            format!("Sound: {} Opus packets played", stats.audio_packets)
                        } else {
                            "Sound: none yet".into()
                        });
                        let mut muted = client.muted(stream.window);
                        if ui.checkbox(&mut muted, "Mute").changed() {
                            client.set_muted(stream.window, muted);
                        }
                    });
                    if let Some(error) = &stats.audio_error {
                        ui.colored_label(ui.visuals().error_fg_color, format!("sound: {error}"));
                    }
                    if let Some(error) = &stats.error {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                }
                ui.separator();

                ui.heading("Clipboard and log");
                ui.label(format!(
                    "Host clipboard: {}",
                    snapshot.clipboard.as_deref().unwrap_or("(nothing shared)")
                ));
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.clipboard);
                    if ui.button("Send to the host").clicked() {
                        let _ = client.set_clipboard(&self.clipboard);
                    }
                });
                for line in snapshot.log.iter().rev() {
                    ui.label(RichText::new(line).monospace().weak());
                }
            });
    }
}

fn encoding_ui(
    ui: &mut egui::Ui,
    host: &HostRole,
    snapshot: &HostSnapshot,
    settings: &mut HostSettings,
) {
    ui.horizontal(|ui| {
        ui.label("Encoder");
        egui::ComboBox::from_id_salt("encoder")
            .selected_text(settings.encoder.clone())
            .show_ui(ui, |ui| {
                for (name, codecs) in &host.encoders {
                    let text = if codecs.is_empty() {
                        format!("{name} (not on this machine)")
                    } else {
                        let list: Vec<&str> = codecs.iter().map(|c| codec_name(*c)).collect();
                        format!("{name} ({})", list.join(", "))
                    };
                    ui.add_enabled_ui(!codecs.is_empty(), |ui| {
                        ui.selectable_value(&mut settings.encoder, (*name).to_owned(), text);
                    });
                }
            });
        ui.label("Codec");
        let codecs = host
            .encoders
            .iter()
            .find(|(name, _)| *name == settings.encoder)
            .map(|(_, codecs)| codecs.clone())
            .unwrap_or_default();
        egui::ComboBox::from_id_salt("codec")
            .selected_text(if settings.codec.is_empty() {
                "Any the client prefers".to_owned()
            } else {
                format!("Only {}", settings.codec)
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut settings.codec, String::new(), "Any the client prefers");
                for codec in codecs {
                    ui.selectable_value(
                        &mut settings.codec,
                        format!("{codec:?}"),
                        format!("Only {}", codec_name(codec)),
                    );
                }
            });
    });
    ui.horizontal(|ui| {
        ui.label("Frame rate");
        ui.add(egui::DragValue::new(&mut settings.fps).range(1..=240));
        ui.label("Bitrate at 1080p");
        ui.add(egui::Slider::new(&mut settings.bitrate_mbps, 1.0..=50.0).suffix(" Mbit/s"));
    });
    let offered: Vec<&str> = snapshot.offered.iter().map(|c| codec_name(*c)).collect();
    ui.label(
        RichText::new(format!(
            "Offering {}. Encoder, frame rate and bitrate apply to running streams at once; a codec change applies to the next stream.",
            offered.join(", ")
        ))
        .weak(),
    );
}

fn stream_window_ui(ui: &mut egui::Ui, client: &ClientRole) {
    let config = client.store().get().client;
    let (mut fullscreen, mut display, mut codec, mut send_input) = (
        config.fullscreen,
        config.display,
        config.codec.clone(),
        config.send_input,
    );
    let displays = windowcast_client_windows::displays();
    ui.horizontal(|ui| {
        ui.checkbox(&mut fullscreen, "Fullscreen");
        ui.label("on");
        egui::ComboBox::from_id_salt("display")
            .selected_text(if display == 0 {
                "the primary display".to_owned()
            } else {
                format!("display {display}")
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut display, 0, "the primary display");
                for d in &displays {
                    ui.selectable_value(
                        &mut display,
                        d.number as usize,
                        format!(
                            "display {} ({}x{}{})",
                            d.number,
                            d.width,
                            d.height,
                            if d.primary { ", primary" } else { "" }
                        ),
                    );
                }
            });
        let decodable = windowcast_client_windows::decodable();
        ui.label("Codec");
        egui::ComboBox::from_id_salt("client-codec")
            .selected_text(if codec.is_empty() {
                "best this PC decodes".to_owned()
            } else {
                codec.clone()
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut codec, String::new(), "best this PC decodes");
                for c in decodable {
                    ui.selectable_value(&mut codec, format!("{c:?}"), codec_name(c));
                }
            });
        ui.checkbox(&mut send_input, "Send pointer and keys");
    });
    let mut lan_only = config.lan_only;
    if ui
        .checkbox(
            &mut lan_only,
            "Only look for hosts on this network (never away from home)",
        )
        .changed()
    {
        client.store().update(|c| c.client.lan_only = lan_only);
    }
    let mut microphone = client.microphone_on();
    if ui
        .checkbox(&mut microphone, "Send my microphone to the host")
        .changed()
    {
        client.set_microphone(microphone);
    }
    if let Some(error) = client.microphone_error() {
        ui.colored_label(ui.visuals().error_fg_color, format!("microphone: {error}"));
    }
    ui.label(
        RichText::new(
            "Each stream opens in its own window; F11 or a double click switches it to fullscreen.",
        )
        .weak(),
    );
    if send_input != config.send_input {
        client.set_send_input(send_input);
    }
    if (fullscreen, display, &codec) != (config.fullscreen, config.display, &config.codec) {
        client.store().update(|c| {
            c.client.fullscreen = fullscreen;
            c.client.display = display;
            c.client.codec = codec;
        });
    }
}

/// A stream's adaptive quality in a few words: what it is held to, at
/// what size and rate, and the network.
fn network(quality: &windowcast_protocol::StreamQuality) -> String {
    let held = match quality.target_kbps {
        Some(kbps) => format!("held to {:.1} Mbit/s", f64::from(kbps) / 1000.0),
        None => "as set".into(),
    };
    format!(
        "{held}, {}x{} at {} fps; loss {:.1}%{}",
        quality.width,
        quality.height,
        quality.fps,
        quality.loss_percent,
        quality
            .rtt_ms
            .map(|ms| format!(", round trip {ms} ms"))
            .unwrap_or_default()
    )
}

/// One ceiling as a number field, 0 meaning none. Returns whether it changed.
fn limit(ui: &mut egui::Ui, value: &mut Option<u32>, unit: &str, max: u32) -> bool {
    let mut number = value.unwrap_or(0);
    let changed = ui
        .add(
            egui::DragValue::new(&mut number)
                .range(0..=max)
                .suffix(format!(" {unit}")),
        )
        .changed();
    if changed {
        *value = (number > 0).then_some(number);
    }
    changed
}

/// The RemoteApp setting (docs/BACKENDS.md, "RemoteApp"): launches the
/// client's rules give RDP run as RemoteApps of this computer's Remote
/// Desktop, with the warning Windows 10 and 11 need.
fn remote_apps_ui(
    ui: &mut egui::Ui,
    snapshot: &crate::host::HostSnapshot,
    settings: &mut crate::config::RemoteAppSettings,
) {
    use crate::config::{RemoteAppAvailability as A, RemoteAppLogin as L};
    let status = match &snapshot.remote_apps {
        Some(Ok(status)) => status,
        // Not Windows: nothing to offer.
        _ => return,
    };
    ui.horizontal(|ui| {
        ui.label("Launches as RemoteApps");
        egui::ComboBox::from_id_salt("remote_apps")
            .selected_text(match settings.availability {
                A::Auto => "automatic",
                A::On => "on",
                A::Off => "off",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut settings.availability, A::Auto, "automatic");
                ui.selectable_value(&mut settings.availability, A::On, "on");
                ui.selectable_value(&mut settings.availability, A::Off, "off");
            });
        ui.label("signing in as");
        egui::ComboBox::from_id_salt("remote_app_login")
            .selected_text(match settings.login {
                L::SignedInUser => "the user who signed in",
                L::HostAccount => "a user this host makes",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut settings.login,
                    L::SignedInUser,
                    "the user who signed in",
                );
                ui.selectable_value(
                    &mut settings.login,
                    L::HostAccount,
                    "a user this host makes",
                );
            });
    });
    let on = match settings.availability {
        A::Auto => status.auto(),
        A::On => true,
        A::Off => false,
    };
    let text = match (settings.availability, status.warning()) {
        (A::Off, _) => None,
        (A::On, Some(warning)) => {
            Some(RichText::new(warning).color(egui::Color32::from_rgb(220, 120, 40)))
        }
        (A::Auto, Some(warning)) => Some(RichText::new(format!("Off for now: {warning}")).weak()),
        (_, None) => {
            Some(RichText::new("On: programs clients start run through Remote Desktop.").weak())
        }
    };
    if let Some(text) = text {
        ui.label(text);
    }
    if on && !status.any_program {
        ui.label(
            RichText::new(
                "Windows runs only programs on its RemoteApp list here; others fail to start.",
            )
            .weak(),
        );
    }
}

fn kind_name(kind: windowcast_protocol::WindowKind) -> &'static str {
    match kind {
        windowcast_protocol::WindowKind::Normal => "window",
        windowcast_protocol::WindowKind::Dialog => "dialog",
        windowcast_protocol::WindowKind::Popup => "popup",
        windowcast_protocol::WindowKind::Menu => "menu",
    }
}

/// The windows in list order, each followed by the windows it owns (and
/// theirs), with how deep it sits. A window whose owner is not listed
/// stands at the top.
fn grouped(windows: &[crate::client::ClientWindow]) -> Vec<(usize, &crate::client::ClientWindow)> {
    fn under<'a>(
        windows: &'a [crate::client::ClientWindow],
        owner: Option<windowcast_protocol::WindowId>,
        depth: usize,
        out: &mut Vec<(usize, &'a crate::client::ClientWindow)>,
    ) {
        if depth > 8 {
            return;
        }
        for window in windows.iter().filter(|w| match owner {
            None => w
                .info
                .owner
                .is_none_or(|o| !windows.iter().any(|x| x.info.id == o)),
            Some(_) => w.info.owner == owner,
        }) {
            if out.iter().any(|(_, w)| w.info.id == window.info.id) {
                continue;
            }
            out.push((depth, window));
            under(windows, Some(window.info.id), depth + 1, out);
        }
    }
    let mut out = Vec::new();
    under(windows, None, 0, &mut out);
    out
}
