use std::{
    collections::HashMap,
    io::Write,
    pin::{Pin, pin},
    time::Duration,
};

use anyhow::{Context, bail};
use cosmic::iced_futures::stream;

use bluer::{AdapterEvent, AdapterProperty, DeviceEvent, DeviceProperty};
use futures::{FutureExt, SinkExt, Stream, StreamExt, TryStreamExt, stream::FuturesUnordered};
use tokio::sync::{mpsc, oneshot};

use crate::{agent::{AgentEvent, create_agent}, device::{BluetoothDevice, DEFAULT_DEVICE_ICON, DeviceUpdate}};

#[derive(Debug, Clone)]
pub enum WorkerEvent {
    Ready(mpsc::UnboundedSender<WorkerRequest>, bool),
    DeviceMap(HashMap<bluer::Address, BluetoothDevice>),
    DeviceAdded(BluetoothDevice),
    DeviceRemoved(bluer::Address),
    ConnectFailed(bluer::Address),
    DeviceUpdate(bluer::Address, DeviceUpdate),
    Enabled(bool),
    Error(String),
    ConfirmCode(String, bluer::Address),
}

#[derive(Debug, Clone)]
pub enum WorkerRequest {
    SetDiscovery(bool),
    ConnectDevice(bluer::Address),
    DisconnectDevice(bluer::Address),
    CancelConnect(bluer::Address),
    SetEnabled(bool),
    ConfirmCode(bluer::Address, bool),
}

// we need to use rfkill to enable/disable bluetooth
#[repr(C, packed)]
struct RfkillEvent {
    idx: u32,
    _type: u8,
    op: u8,
    soft: u8,
    hard: u8,
}

/// background worker struct, All calls to bluer and async code lives here
/// listens for requests from the model, events from the adapter, and events for each of the devices
struct BluetoothWorker {
    output: futures::channel::mpsc::Sender<WorkerEvent>,
    requests: mpsc::UnboundedReceiver<WorkerRequest>,
    adapter: bluer::Adapter,
    adapter_events: Pin<Box<dyn Stream<Item = bluer::AdapterEvent> + Send>>,
    discovery_events: Option<Pin<Box<dyn Stream<Item = bluer::AdapterEvent> + Send>>>,
    device_rx: mpsc::UnboundedReceiver<(bluer::Address, DeviceUpdate)>,
    device_tx: mpsc::UnboundedSender<(bluer::Address, DeviceUpdate)>,
    device_handles: HashMap<bluer::Address, tokio::task::JoinHandle<()>>,
    agent_handle: bluer::agent::AgentHandle,
    agent_rx: mpsc::UnboundedReceiver<AgentEvent>,
    confirmation_senders: HashMap<bluer::Address, oneshot::Sender<bool>>,
}

impl BluetoothWorker {
    async fn try_create(
        mut output: futures::channel::mpsc::Sender<WorkerEvent>,
    ) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();

        let (adapter, session) = get_connection().await?;

        let (agent_tx, agent_rx) = mpsc::unbounded_channel();
        let agent = create_agent(agent_tx);
        let agent_handle = session.register_agent(agent).await?;

        let adapter_events = adapter.events().await?.boxed();

        let (device_tx, device_rx) = mpsc::unbounded_channel();

        let (bt_device_map, device_handles) = create_device_maps(&adapter, &device_tx).await?;

        let enabled = adapter.is_powered().await?;

        _ = output.send(WorkerEvent::Ready(tx, enabled)).await;
        _ = output.send(WorkerEvent::DeviceMap(bt_device_map)).await;

        Ok(BluetoothWorker {
            output,
            requests: rx,
            adapter,
            adapter_events,
            discovery_events: None,
            device_handles,
            device_rx,
            device_tx,
            agent_handle,
            agent_rx,
            confirmation_senders: HashMap::new(),
        })
    }

    async fn run(mut self) {
        loop {
            if let Err(e) = self.listen().await {
                _ = self.output.send(WorkerEvent::Error(format!{"{:?}", e})).await;
                return;
            }
        }
    }

    async fn handle_adapter_event(&mut self, event: AdapterEvent) -> anyhow::Result<()> {
        let message = match event {
            AdapterEvent::PropertyChanged(AdapterProperty::Powered(v)) => WorkerEvent::Enabled(v),
            AdapterEvent::DeviceRemoved(addr) => {
                // DeviceAdded and DeviceRemoved fire both when a device connects/disconnects, and when a device is 
                // added/removed from the adapter database, this is the only way to distinguish between them 🙄
                if self.adapter.device_addresses().await?.contains(&addr) {
                    return Ok(())
                }

                if let Some(handle) = self.device_handles.remove(&addr) {
                    handle.abort();
                    WorkerEvent::DeviceRemoved(addr)
                } else {
                    return Ok(())
                }
            }
            AdapterEvent::DeviceAdded(addr) => {
                let device = self.adapter.device(addr)?;

                if device.name().await?.is_none() || self.device_handles.contains_key(&addr) {
                    return Ok(());
                }

                let addr_ = addr.clone();
                let output_ = self.device_tx.clone();
                let events = device.events().await?;

                let handle =
                    tokio::spawn(async move { device_listener(addr_, events, output_).await });

                self.device_handles.insert(addr.clone(), handle);

                let device = BluetoothDevice::from_device(&device).await;
                WorkerEvent::DeviceAdded(device)
            }
            _ => return Ok(()),
        };

        _ = self.output.send(message).await;
        Ok(())
    }

    async fn handle_agent_event(&mut self, event: AgentEvent) -> anyhow::Result<()> {
        match event {
            AgentEvent::RequestConfirmation(passkey, addr, output) => {
                tracing::info!("worker received confirmation request...");
                self.confirmation_senders.insert(addr.clone(), output);
                _ = self.output.send(WorkerEvent::ConfirmCode(passkey.to_string(), addr)).await;
            }
        }

        Ok(())
    }

    async fn handle_request(&mut self, request: WorkerRequest) -> anyhow::Result<()> {
        match request {
            WorkerRequest::SetDiscovery(v) => {
                if v && self.adapter.is_powered().await? {
                    self.discovery_events = Some(self.adapter.discover_devices().await?.boxed());
                    tracing::info!("started device discovery")
                } else {
                    self.discovery_events = None;
                    tracing::info!("stopped device discovery")
                }
            }
            WorkerRequest::ConnectDevice(addr) => {
                let device = self.adapter.device(addr)?;
                let mut output = self.output.clone();
                tokio::spawn(async move {
                    if let Err(e) = connect_with_retry(&device).await {
                        tracing::error!("device failed to connect: {e}");
                        _ = output.send(WorkerEvent::ConnectFailed(device.address())).await
                    }
                });
            }
            WorkerRequest::DisconnectDevice(addr) => {
                let device = self.adapter.device(addr)?;
                tokio::spawn(async move {
                    if let Err(e) = device.disconnect().await {
                        tracing::warn!("device failed to disconnect: {e}");
                    }
                });
            }
            WorkerRequest::CancelConnect(addr) => {
                let device = self.adapter.device(addr)?;
                let mut output = self.output.clone();
                tokio::spawn(async move {
                    if let Err(e) = device.disconnect().await {
                        tracing::warn!("device failed to disconnect: {e}");
                    }
                    _ = output.send(WorkerEvent::ConnectFailed(device.address())).await
                });
            }
            WorkerRequest::SetEnabled(enabled) => {
                tracing::info!("Setting bluetooth enabled to {}", enabled);

                if self.adapter.set_powered(enabled).await.is_ok() {
                    return Ok(())
                }

                let idx = find_adapter_idx(self.adapter.name())?;

                rfkill_set_enabled(idx, enabled)?;

                if enabled {
                    let (bt_device_map, device_handles) =
                        create_device_maps(&self.adapter, &self.device_tx).await?;

                    _ = std::mem::replace(&mut self.device_handles, device_handles);

                    _ = self
                        .output
                        .send(WorkerEvent::DeviceMap(bt_device_map))
                        .await;
                } else {
                    self.device_handles.drain().for_each(|(_, h)| h.abort());
                }
            },
            WorkerRequest::ConfirmCode(addr, confirm) => {
                if let Some(sender) = self.confirmation_senders.remove(&addr) {
                    _ = sender.send(confirm)
                }
            }
        }
        Ok(())
    }

    async fn listen(&mut self) -> anyhow::Result<()> {
        tokio::select! {
            Some(r) = self.requests.recv() => self.handle_request(r.clone()).await
                .context(format!("Could not handle request: {:?}", r)),
            Some(e) = self.adapter_events.next() => self.handle_adapter_event(e.clone()).await
                .context(format!("Could not handle adapter event: {:?}", e)),
            Some(e) = async {
                match self.discovery_events.as_mut() {
                    Some(stream) => stream.next().await,
                    None => futures::future::pending().await, // Never resolves
                } 
            } => {
                self.handle_adapter_event(e.clone()).await
                    .context(format!("Could not handle discovery event: {:?}", e))
            },
            Some((a, u)) = self.device_rx.recv() => {
                _ = self.output.send(WorkerEvent::DeviceUpdate(a, u)).await;
                Ok(())
            },
            Some(e) = self.agent_rx.recv() => self.handle_agent_event(e).await,
        }
    }
}

async fn get_connection() -> anyhow::Result<(bluer::Adapter, bluer::Session)> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;

    Ok((adapter, session))
}

pub fn spawn_worker() -> impl Stream<Item = WorkerEvent> {
    stream::channel(50, async move |mut output| {
        let output_ = output.clone();
        let worker = match BluetoothWorker::try_create(output_)
            .await
            .context("Could not create worker state")
        {
            Ok(w) => w,
            Err(e) => {
                _ = output.send(WorkerEvent::Error(format!{"{:?}", e})).await;
                return;
            }
        };

        worker.run().await;
    })
}

async fn device_listener(
    addr: bluer::Address,
    events: impl Stream<Item = DeviceEvent>,
    output: mpsc::UnboundedSender<(bluer::Address, DeviceUpdate)>,
) {
    let mut pinned_events = pin!(events);

    while let Some(DeviceEvent::PropertyChanged(p)) = pinned_events.next().await {
        let message = match p {
            DeviceProperty::BatteryPercentage(battery) => DeviceUpdate::Battery(battery),
            DeviceProperty::Connected(connected) => DeviceUpdate::Connected(connected),
            DeviceProperty::Paired(paired) => DeviceUpdate::Paired(paired),
            _ => continue,
        };

        let addr_ = addr.clone();
        _ = output.send((addr_, message))
    }
}

async fn connect_with_retry(device: &bluer::Device) -> anyhow::Result<()> {
    const MAX_TRIES: u32 = 5;
    let mut attempt = 0;
    let mut backoff = Duration::from_millis(500);

    loop {
        attempt += 1;

        match device.connect().await {
            Ok(_) => return Ok(()),
            Err(e) => {
                if attempt >= MAX_TRIES {
                    bail!(e)
                }

                // Exponential backoff with max of 10 seconds
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        }
    }
}

fn find_adapter_idx(adapter_name: &str) -> anyhow::Result<u32> {
    for entry in std::fs::read_dir("/sys/class/rfkill")? {
        let entry = entry?;
        let path = entry.path();

        let is_bluetooth = std::fs::read_to_string(path.join("type"))
            .map(|t| t.trim() == "bluetooth")
            .unwrap_or(false);

        if !is_bluetooth {
            continue;
        }

        let name_match = std::fs::read_to_string(path.join("name"))
            .map(|t| t.trim() == adapter_name)
            .unwrap_or(false);

        if name_match {
            let idx = std::fs::read_to_string(path.join("index"))?
                .trim()
                .parse::<u32>()?;

            return Ok(idx);
        }
    }

    bail!("No rfkill bluetooth device with name {}", adapter_name)
}

fn rfkill_set_enabled(idx: u32, enable: bool) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/rfkill")?;

    // see https://github.com/torvalds/linux/blob/master/include/uapi/linux/rfkill.h
    let event = RfkillEvent {
        idx,
        _type: 2, // RFKILL_TYPE_BLUETOOTH
        op: 2,    // RFKILL_OP_CHANGE,
        soft: if enable { 0 } else { 1 },
        hard: 0,
    };

    let bytes: [u8; std::mem::size_of::<RfkillEvent>()] = unsafe { std::mem::transmute(event) };
    file.write_all(&bytes)?;

    Ok(())
}

async fn create_device_maps(
    adapter: &bluer::Adapter,
    device_tx: &mpsc::UnboundedSender<(bluer::Address, DeviceUpdate)>,
) -> anyhow::Result<(
    HashMap<bluer::Address, BluetoothDevice>,
    HashMap<bluer::Address, tokio::task::JoinHandle<()>>,
)> {
    let mut futures = adapter
        .device_addresses()
        .await?
        .into_iter()
        .map(async |addr| {
            let device = adapter.device(addr)?;
            let bt_device = BluetoothDevice::from_device(&device).await;

            if device.name().await?.is_none() && bt_device.icon == DEFAULT_DEVICE_ICON {
                return Ok(None);
            }

            let events = device.events().await?;
            let addr_ = addr.clone();
            let output = device_tx.clone();
            Ok::<_, bluer::Error>(Some((
                addr,
                bt_device,
                tokio::spawn(async move { device_listener(addr_, events, output).await }),
            )))
        })
        .collect::<FuturesUnordered<_>>();

    let mut device_handles = HashMap::new();
    let mut device_map = HashMap::new();

    while let Some((addr, bt_device, handle)) = futures.try_next().await?.flatten() {
        device_map.insert(addr.clone(), bt_device);
        device_handles.insert(addr, handle);
    }

    Ok((device_map, device_handles))
}
