use futures::{FutureExt};

/// a mirror/cache of the bluer device struct, recieves updates from worker
#[derive(Debug, Clone)]
pub struct BluetoothDevice {
    pub icon: &'static str,
    pub name: String,
    pub status: ConnectionStatus,
    pub battery_percent: Option<u8>,
    pub is_paired: bool,
    pub address: bluer::Address,
    pub display_code: Option<String>,
}

#[derive(Debug, Clone)]
pub enum DeviceUpdate {
    Connected(bool),
    Battery(u8),
    Paired(bool),
}

#[derive(Debug, Clone, Copy)]
pub enum ConnectionStatus {
    Connected,
    Connecting,
    Disconnected,
    Disconnecting
}

pub const DEFAULT_DEVICE_ICON: &'static str = "bluetooth-symbolic";

// Copied from https://github.com/bluez/bluez/blob/39467578207889fd015775cbe81a3db9dd26abea/src/dbus-common.c#L53
fn device_type_to_icon(device_type: &str) -> &'static str {
    match device_type {
        "computer" => "laptop-symbolic",
        "phone" => "smartphone-symbolic",
        "network-wireless" => "network-wireless-symbolic",
        "audio-headset" => "audio-headset-symbolic",
        "audio-headphones" => "audio-headphones-symbolic",
        "camera-video" => "camera-video-symbolic",
        "audio-card" => "audio-card-symbolic",
        "input-gaming" => "input-gaming-symbolic",
        "input-keyboard" => "input-keyboard-symbolic",
        "input-tablet" => "input-tablet-symbolic",
        "input-mouse" => "input-mouse-symbolic",
        "printer" => "printer-network-symbolic",
        "camera-photo" => "camera-photo-symbolic",
        _ => "bluetooth-symbolic",
    }
}

impl BluetoothDevice {
    pub async fn from_device(device: &bluer::Device) -> Self {
        let (
        mut name, is_paired, _is_trusted, is_connected, battery_percent, icon) = futures::join!(
            device.name().map(|res| res.ok().flatten().unwrap_or_default()),
            device.is_paired().map(Result::unwrap_or_default),
            device.is_trusted().map(Result::unwrap_or_default),
            device.is_connected().map(Result::unwrap_or_default),
            device.battery_percentage().map(|res| res.ok().flatten()),
            device
                .icon()
                .map(|res| device_type_to_icon(&res.ok().flatten().unwrap_or_default()))
        );

        if name.is_empty() {
            name = device.address().to_string();
        }

        let status = if is_connected {
            ConnectionStatus::Connected
        } else {
            ConnectionStatus::Disconnected
        };

        Self {
            name,
            icon,
            status,
            battery_percent,
            is_paired,
            address: device.address(),
            display_code: None,
        }
    }

    pub fn handle_device_updates(&mut self, update: DeviceUpdate) {
        match update {
            DeviceUpdate::Battery(battery) => self.battery_percent = Some(battery),
            DeviceUpdate::Paired(paired) => self.is_paired = paired,
            DeviceUpdate::Connected(connected) => {
                self.status = if connected {
                    ConnectionStatus::Connected
                } else {
                    ConnectionStatus::Disconnected
                }
            }
        }
    }
}