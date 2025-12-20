use std::{collections::HashMap, sync::LazyLock};

use crate::{
    config,
    device::{BluetoothDevice, ConnectionStatus},
    fl,
    worker::{self, WorkerEvent, WorkerRequest},
};
use cosmic::{
    Element,
    app::Task,
    applet::{
        menu_button, padded_control,
        token::subscription::{self, TokenRequest, TokenUpdate},
    },
    cctk::sctk::reexports::calloop,
    iced::{Subscription, platform_specific::shell::wayland::commands::popup},
    iced_core::{Alignment, Length, window},
    iced_widget::{Column, column, row, scrollable},
    widget::{button, container, divider, icon, text},
};
use cosmic_time::{Instant, Timeline, anim, id};
use tokio::sync::mpsc;

static BLUETOOTH_ENABLED: LazyLock<id::Toggler> = LazyLock::new(id::Toggler::unique);

pub fn run() -> cosmic::iced::Result {
    cosmic::applet::run::<CosmicBluetoothApplet>(())
}

#[derive(Default)]
struct CosmicBluetoothApplet {
    core: cosmic::app::Core,
    device_map: Option<HashMap<bluer::Address, BluetoothDevice>>,
    enabled: bool,
    worker_tx: Option<mpsc::UnboundedSender<WorkerRequest>>,
    token_tx: Option<calloop::channel::Sender<TokenRequest>>,

    // UI state
    popup: Option<window::Id>,
    show_visible_devices: bool,
    timeline: Timeline,
}

#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    OpenSettings,
    ToggleBluetooth(cosmic_time::chain::Toggler, bool),
    ToggleVisibleDevices(bool),
    Frame(Instant),
    BluetoothEvent(WorkerEvent),
    Token(TokenUpdate),
    Request(WorkerRequest),
    CloseRequested(window::Id),
    ConfirmCode(bluer::Address, bool),
}

impl CosmicBluetoothApplet {
    fn handle_worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Ready(tx, e) => {
                self.worker_tx = Some(tx);
                self.enabled = e;
            }
            WorkerEvent::DeviceMap(m) => self.device_map = Some(m),
            WorkerEvent::Error(err) => {
                eprintln!("Bluetooth worker failed with error: {}. Exiting...", err);
                tracing::error!("Bluetooth worker failed with error: {}. Exiting...", err);
                std::process::exit(1);
            }
            WorkerEvent::DeviceAdded(device) => {
                self.device_map
                    .as_mut()
                    .map(|d| d.insert(device.address.clone(), device));
            }
            WorkerEvent::DeviceRemoved(addr) => {
                tracing::info!("Device removed: {}", addr);
                self.device_map.as_mut().map(|d| d.remove(&addr));
            }
            WorkerEvent::Enabled(true) => {
                self.enabled = true;

                if self.popup.is_some()
                    && let Some(tx) = self.worker_tx.as_ref()
                {
                    _ = tx.send(WorkerRequest::SetDiscovery(true));
                }
            }
            WorkerEvent::Enabled(false) => {
                self.enabled = false;
            }
            WorkerEvent::DeviceUpdate(addr, update) => {
                self.device_map.as_mut().map(|d| {
                    if let Some(dev) = d.get_mut(&addr) {
                        dev.handle_device_updates(update);
                    } else {
                        tracing::warn!("Bluetooth worker and app model are out of sync!")
                    }
                });
            }
            WorkerEvent::ConnectFailed(addr) => {
                self.device_map.as_mut().map(|d| {
                    if let Some(dev) = d.get_mut(&addr) {
                        dev.status = ConnectionStatus::Disconnected;
                    } else {
                        tracing::warn!("Bluetooth worker and app model are out of sync!")
                    }
                });
            }
            WorkerEvent::ConfirmCode(code, addr) => {
                self.device_map.as_mut().map(|d| {
                    if let Some(dev) = d.get_mut(&addr) {
                        dev.display_code = Some(code)
                    } else {
                        tracing::warn!("Bluetooth worker and app model are out of sync!")
                    }
                });
            }
        }
    }
}

impl cosmic::Application for CosmicBluetoothApplet {
    type Message = Message;
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    const APP_ID: &'static str = config::APP_ID;

    fn init(core: cosmic::Core, _flags: Self::Flags) -> (Self, Task<Self::Message>) {
        (
            Self {
                core,
                ..Default::default()
            },
            cosmic::task::none(),
        )
    }

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::CloseRequested(id))
    }

    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            Message::BluetoothEvent(ev) => self.handle_worker_event(ev),
            Message::Request(worker_request) => {
                if let Some(worker_tx) = self.worker_tx.as_mut() {
                    if let Some(device_map) = self.device_map.as_mut()
                        && let WorkerRequest::ConnectDevice(addr)
                        | WorkerRequest::DisconnectDevice(addr) = worker_request
                    {
                        if let Some(dev) = device_map.get_mut(&addr) {
                            match worker_request {
                                WorkerRequest::ConnectDevice(_) => {
                                    dev.status = ConnectionStatus::Connecting
                                }
                                WorkerRequest::DisconnectDevice(_) => {
                                    dev.status = ConnectionStatus::Disconnecting
                                }
                                _ => {}
                            }
                        } else {
                            tracing::warn!("Bluetooth worker and app model are out of sync!")
                        }
                    }

                    _ = worker_tx.send(worker_request)
                }
            }
            Message::TogglePopup => {
                let task = if let Some(p) = self.popup.take() {
                    popup::destroy_popup(p)
                } else {
                    // TODO request update of state maybe
                    let new_id = window::Id::unique();
                    self.popup.replace(new_id);
                    self.timeline = Timeline::new();

                    let popup_settings = self.core.applet.get_popup_settings(
                        self.core.main_window_id().unwrap(),
                        new_id,
                        None,
                        None,
                        None,
                    );

                    popup::get_popup(popup_settings)
                };

                let popup_open = self.popup.is_some();
                if let Some(worker_tx) = self.worker_tx.as_ref() {
                    _ = worker_tx.send(WorkerRequest::SetDiscovery(popup_open));
                }

                return task;
            }
            Message::OpenSettings => {
                let exec = "cosmic-settings bluetooth".to_string();
                if let Some(tx) = self.token_tx.as_ref() {
                    let _ = tx.send(subscription::TokenRequest {
                        app_id: Self::APP_ID.to_string(),
                        exec,
                    });
                }
            }
            Message::Token(u) => match u {
                TokenUpdate::Init(tx) => {
                    self.token_tx = Some(tx);
                }
                TokenUpdate::Finished => {
                    self.token_tx = None;
                }
                TokenUpdate::ActivationToken { token, .. } => {
                    let mut cmd = std::process::Command::new("cosmic-settings");
                    cmd.arg("bluetooth");
                    if let Some(token) = token {
                        cmd.env("XDG_ACTIVATION_TOKEN", &token);
                        cmd.env("DESKTOP_STARTUP_ID", &token);
                    }
                    tokio::spawn(cosmic::process::spawn(cmd));
                }
            },
            Message::Frame(instant) => self.timeline.now(instant),
            Message::ToggleBluetooth(chain, enabled) => {
                self.timeline.set_chain(chain).start();
                if let Some(tx) = self.worker_tx.as_mut() {
                    _ = tx.send(WorkerRequest::SetEnabled(enabled));
                }
            }
            Message::ToggleVisibleDevices(enabled) => {
                self.show_visible_devices = enabled;
            }
            Message::CloseRequested(_id) => {
                self.popup = None;
                if let Some(worker_tx) = self.worker_tx.as_ref() {
                    _ = worker_tx.send(WorkerRequest::SetDiscovery(false));
                }
            }
            Message::ConfirmCode(addr, confirm) => {
                if let Some(worker_tx) = self.worker_tx.as_ref() {
                    _ = worker_tx.send(WorkerRequest::ConfirmCode(addr, confirm));
                }
            }
        };
        Task::none()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        Subscription::batch([
            subscription::activation_token_subscription(0).map(Message::Token),
            Subscription::run(worker::spawn_worker).map(Message::BluetoothEvent),
            self.timeline
                .as_subscription()
                .map(|(_, now)| Message::Frame(now)),
        ])
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let icon_name = if self.enabled {
            "cosmic-applet-bluetooth-active-symbolic"
        } else {
            "cosmic-applet-bluetooth-disabled-symbolic"
        };

        self.core
            .applet
            .icon_button(icon_name)
            .on_press_down(Message::TogglePopup)
            .into()
    }

    fn style(&self) -> Option<cosmic::iced_runtime::Appearance> {
        Some(cosmic::applet::style())
    }

    fn view_window(&self, _id: cosmic::iced::window::Id) -> Element<'_, Self::Message> {
        let cosmic::cosmic_theme::Spacing {
            space_xxs, space_s, ..
        } = cosmic::theme::active().cosmic().spacing;

        let (paired, unpaired) = if let Some(device_map) = self.device_map.as_ref() {
            let (mut paired, mut unpaired): (Vec<&BluetoothDevice>, Vec<&BluetoothDevice>) =
                device_map.values().partition(|d| d.is_paired);

            paired.sort_by_key(|f| &f.name);
            unpaired.sort_by_key(|f| &f.name);

            (paired, unpaired)
        } else {
            (vec![], vec![])
        };

        // build list of paired bluetooth devices
        let paired: Vec<Element<'_, Message>> = paired
            .into_iter()
            .map(|dev| {
                let mut row = row![
                    icon::from_name(dev.icon).size(16).symbolic(true),
                    text::body(dev.name.as_str())
                        .align_x(Alignment::Start)
                        .align_y(Alignment::Center)
                        .width(Length::Fill)
                ]
                .align_y(Alignment::Center)
                .spacing(12);

                if let Some(battery) = dev.battery_percent {
                    let icon = match battery {
                        b if (20..40).contains(&b) => "battery-low",
                        b if b < 20 => "battery-caution",
                        _ => "battery",
                    };
                    let status = row!(
                        icon::from_name(icon).symbolic(true).size(14),
                        text::body(format!("{battery}%"))
                    )
                    .align_y(Alignment::Center)
                    .spacing(2)
                    .width(Length::Shrink);

                    let content = container(status)
                        .align_x(Alignment::End)
                        .align_y(Alignment::Center);

                    row = row.push(content);
                }

                match dev.status {
                    ConnectionStatus::Connected => {
                        row = row.push(
                            text::body(fl!("connected"))
                                .align_x(Alignment::End)
                                .align_y(Alignment::Center),
                        );
                    }
                    ConnectionStatus::Connecting | ConnectionStatus::Disconnecting => {
                        // TODO make more consistent with spinning icon on cosmic-greeter?
                        row = row.push(
                            icon::from_name("process-working-symbolic")
                                .size(24)
                                .symbolic(true),
                        );
                    }
                    ConnectionStatus::Disconnected => {}
                }

                let mut button = menu_button(row);
                match dev.status {
                    ConnectionStatus::Connected => {
                        button = button.on_press(Message::Request(WorkerRequest::DisconnectDevice(
                            dev.address,
                        )))
                    }
                    ConnectionStatus::Connecting => {
                        button = button
                            .on_press(Message::Request(WorkerRequest::CancelConnect(dev.address)))
                    }
                    ConnectionStatus::Disconnected => {
                        button = button
                            .on_press(Message::Request(WorkerRequest::ConnectDevice(dev.address)))
                    }
                    _ => {}
                }

                button.into()
            })
            .collect();

        let mut content = column![padded_control(anim!(
            BLUETOOTH_ENABLED,
            &self.timeline,
            fl!("bluetooth"),
            self.enabled,
            Message::ToggleBluetooth,
        ))]
        .align_x(Alignment::Center)
        .padding([8, 0]);

        if !paired.is_empty() {
            content = content.extend([
                padded_control(divider::horizontal::default())
                    .padding([space_xxs, space_s])
                    .into(),
                Column::with_children(paired).into(),
            ])
        }

        let dropdown_icon = if self.show_visible_devices {
            "go-up-symbolic"
        } else {
            "go-down-symbolic"
        };

        let mut list_column: Vec<Element<'_, Message>> = Vec::new();

        if self.enabled {
            let available_connections_btn = menu_button(row![
                text::body(fl!("other-devices"))
                    .width(Length::Fill)
                    .height(Length::Fixed(24.0))
                    .align_y(Alignment::Center),
                container(icon::from_name(dropdown_icon).size(16).symbolic(true))
                    .center(Length::Fixed(24.0))
            ])
            .on_press(Message::ToggleVisibleDevices(!self.show_visible_devices));

            content = content.extend([
                padded_control(divider::horizontal::default())
                    .padding([space_xxs, space_s])
                    .into(),
                available_connections_btn.into(),
            ]);

            list_column.extend(unpaired.into_iter().map(|dev| {
                if let Some(code) = dev.display_code.as_ref() {
                    column![
                        padded_control(
                            row![
                                icon::from_name(dev.icon).size(16).symbolic(true),
                                text::body(dev.name.clone()).align_x(Alignment::Start),
                            ]
                            .align_y(Alignment::Center)
                            .spacing(12)
                        ),
                        padded_control(
                            text::body(fl!(
                                "confirm-pin",
                                HashMap::from([("deviceName", dev.name.clone())])
                            ))
                            .align_x(Alignment::Start)
                            .align_y(Alignment::Center)
                            .width(Length::Fill)
                        ),
                        padded_control(text::title3(code).center().width(Length::Fixed(280.0)))
                            .align_x(Alignment::Center),
                        padded_control(
                            row![
                                button::custom(text::body(fl!("cancel")).center())
                                    .padding([4, 0])
                                    .height(Length::Fixed(28.0))
                                    .width(Length::Fixed(105.0))
                                    .on_press(Message::ConfirmCode(dev.address, false)),
                                button::custom(text::body(fl!("confirm")).center())
                                    .padding([4, 0])
                                    .height(Length::Fixed(28.0))
                                    .width(Length::Fixed(105.0))
                                    .on_press(Message::ConfirmCode(dev.address, true)),
                            ]
                            .spacing(self.core.system_theme().cosmic().space_xxs())
                            .width(Length::Shrink)
                            .align_y(Alignment::Center)
                        )
                        .align_x(Alignment::Center)
                    ]
                    .into()
                } else {
                    let row = row![
                        icon::from_name(dev.icon).size(16).symbolic(true),
                        text::body(dev.name.clone())
                            .align_x(Alignment::Start)
                    ]
                    .align_y(Alignment::Center)
                    .spacing(12);

                    menu_button(row.width(Length::Fill))
                        .on_press(Message::Request(WorkerRequest::ConnectDevice(dev.address)))
                        .into()
                }
            }))
        }

        if list_column.len() > 10 {
            content = content
                .push(scrollable(Column::with_children(list_column)).height(Length::Fixed(300.0)));
        } else {
            content = content.extend(list_column);
        }

        content = content.extend([
            padded_control(divider::horizontal::default())
                .padding([space_xxs, space_s])
                .into(),
            menu_button(text::body(fl!("settings")))
                .on_press(Message::OpenSettings)
                .into(),
        ]);

        self.core.applet.popup_container(content).into()
    }
}
