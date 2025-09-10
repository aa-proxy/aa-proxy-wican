use anyhow::{anyhow, Context, Result};
use bluer::gatt::remote::Characteristic;
use bluer::{
    agent::{Agent, AgentHandle},
    Adapter, AdapterEvent, Address, Device, Session, Uuid,
};
use clap::{Parser, ValueEnum};
use futures_util::stream::StreamExt;
use log::{debug, error, info, warn, LevelFilter};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use simplelog::*;
use std::fs::File;
use std::time::Duration;
use tokio::time;

// WiCAN UUIDs
const WICAN_NOTIFY_UUID: Uuid = Uuid::from_u128(0x0200dec0_01ef_bc9a_5678_1234deadf0be);
const WICAN_WRITE_UUID: Uuid = Uuid::from_u128(0x0300dec0_01ef_bc9a_5678_1234deadf0be);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl From<LogLevel> for LevelFilter {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Off => LevelFilter::Off,
            LogLevel::Error => LevelFilter::Error,
            LogLevel::Warn => LevelFilter::Warn,
            LogLevel::Info => LevelFilter::Info,
            LogLevel::Debug => LevelFilter::Debug,
            LogLevel::Trace => LevelFilter::Trace,
        }
    }
}

#[derive(Debug, Deserialize)]
struct WicanResponse {
    #[serde(alias = "SOC")]
    soc: f32,
    #[serde(alias = "SOC_D")]
    soc_d: Option<f32>,
    #[serde(alias = "TMP_A")]
    outdoor_temperature: Option<f32>,
}

#[derive(Parser, Debug, Serialize, Deserialize, Default)]
pub struct BatteryData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery_level_percentage: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery_level_wh: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_air_density: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_temp_celsius: Option<f32>,
    pub battery_capacity_wh: Option<u32>,
}

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
pub struct Configuration {
    /// Vehicle Battery Capacity in wh
    #[arg(short, long)]
    pub vehicle_battery_capacity: u32,

    /// WiCAN MAC address
    #[arg(short, long)]
    pub wican_mac_address: Address,

    /// WiCAN passkey
    #[arg(long, default_value_t = 123456)]
    pub wican_passkey: u32,

    /// WiCAN retries
    #[arg(long, default_value_t = 5)]
    pub wican_max_connect_retries: u8,

    /// WiCAN timeout
    #[arg(long, default_value_t = 10)]
    pub wican_timeout: u8,

    /// WiCAN update frequency in minutes
    #[arg(long, default_value_t = 1)]
    pub wican_update_frequency_minutes: u8,

    /// aa-proxy-rs url
    #[arg(long, default_value = "http://localhost/battery")]
    pub api_url: String,

    /// Log file
    #[arg(long, default_value = "/var/log/aa-proxy-wican.log")]
    pub log_file: String,

    /// Log level
    #[arg(long, value_enum, default_value_t = LogLevel::Info)]
    pub log_level: LogLevel,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse the command line
    let configuration = Configuration::parse();

    // Set log level from command line
    let log_level = LevelFilter::from(configuration.log_level);

    // Confirm we can write to the log file
    let log_file_result = File::create(&configuration.log_file);
    let log_file = match log_file_result {
        Ok(file) => file,
        Err(e) => {
            return Err(anyhow!(
                "Could not start logging to file '{}': {}",
                configuration.log_file,
                e
            ));
        }
    };

    // Create a logger configuration
    let log_config = ConfigBuilder::new()
        .set_target_level(LevelFilter::Info)
        .set_target_level(LevelFilter::Error)
        .set_location_level(LevelFilter::Error)
        .set_time_level(LevelFilter::Error)
        .set_level_padding(LevelPadding::Right)
        .set_time_offset_to_local()
        .expect("Failed to set local time offset")
        .build();

    // Initialize the logger.
    match CombinedLogger::init(vec![
        TermLogger::new(
            log_level,
            log_config.clone(),
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ),
        WriteLogger::new(log_level, log_config.clone(), log_file),
    ]) {
        Ok(_) => {}
        Err(e) => {
            return Err(anyhow!("Could not initialize combined logger: {}", e));
        }
    }

    info!(
        "WiCAN Client starting. Update frequency is {} minute(s).",
        configuration.wican_update_frequency_minutes
    );

    let mut first_run = true;
    loop {
        if !first_run {
            info!(
                "Sleeping for {} minute(s) before next update...",
                configuration.wican_update_frequency_minutes
            );
            time::sleep(Duration::from_secs(
                (configuration.wican_update_frequency_minutes as u64) * 60,
            ))
            .await;
        }
        first_run = false;

        let wican_timeout = Duration::from_secs(configuration.wican_timeout as u64);
        let session = Session::new().await?;
        let adapter = session.default_adapter().await?;

        let device = match connect_to_device(
            session,
            adapter,
            configuration.wican_mac_address,
            configuration.wican_passkey,
            wican_timeout,
            configuration.wican_max_connect_retries,
        )
        .await
        {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to connect to device: {}. Will retry...", e);
                continue;
            }
        };

        if let Some(battery_data) = match fetch_data(
            &device,
            configuration.vehicle_battery_capacity,
            wican_timeout,
        )
        .await
        {
            Ok(data) => data,
            Err(e) => {
                error!("Failed to fetch data from device: {}. Will retry...", e);
                continue;
            }
        } {
            if let Err(e) = post_battery_data(&configuration.api_url, &battery_data).await {
                error!("Failed to post battery data: {}. Will retry...", e);
            }
        }
    }
}

// Finds the target Bluetooth device by its MAC address during a discovery scan.
async fn find_device(
    adapter: &Adapter,
    wican_mac_address: Address,
    wican_timeout: Duration,
) -> Result<Device> {
    if adapter
        .device(wican_mac_address)?
        .is_services_resolved()
        .await
        .is_ok()
    {
        info!("Device {} is known and available.", wican_mac_address);
        return Ok(adapter.device(wican_mac_address)?);
    }

    info!(
        "Starting device discovery to find {} for a maximum of {:?}",
        wican_mac_address, wican_timeout
    );
    let mut device_events = adapter.discover_devices().await?;

    match tokio::time::timeout(wican_timeout, async {
        loop {
            if let Some(AdapterEvent::DeviceAdded(addr)) = device_events.next().await {
                if addr == wican_mac_address {
                    info!("Found device with address: {}", addr);
                    break Ok(adapter.device(addr)?);
                }
            }
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(anyhow!("Scan timed out without finding device.")),
    }
}

// Attempts to pair with the device if it is not already paired.
async fn try_pair(session: &Session, device: &Device, wican_passkey: u32) -> Result<()> {
    if device.is_paired().await? {
        info!("Device is already paired. Skipping pairing.");
        return Ok(());
    }

    let agent = Agent {
        request_default: true,
        request_passkey: Some(Box::new(move |_path| {
            Box::pin(async move {
                info!(
                    "A device requested a passkey code. We're providing '{}'.",
                    wican_passkey
                );
                Ok(wican_passkey)
            })
        })),
        ..Default::default()
    };
    let _agent_handle: AgentHandle = session.register_agent(agent).await?;

    info!("Attempting to pair with device...");
    device.pair().await.context("Failed to pair with device")?;

    info!("Pairing successful!");
    Ok(())
}

// Connects to wican device
async fn connect_to_device(
    session: Session,
    adapter: Adapter,
    wican_mac_address: Address,
    wican_passkey: u32,
    wican_timeout: Duration,
    max_retries: u8,
) -> Result<Device> {
    let device = find_device(&adapter, wican_mac_address, wican_timeout).await?;

    try_pair(&session, &device, wican_passkey).await?;

    if device.is_connected().await? {
        info!("Device is already connected. Skipping connection.");
        return Ok(device);
    }

    for i in 0..max_retries {
        info!(
            "Connecting to device... (Attempt {}/{})",
            i + 1,
            max_retries
        );
        match device.connect().await {
            Ok(_) => {
                info!("Connected successfully!");
                break;
            }
            Err(e) => {
                if i + 1 < max_retries {
                    warn!("Connection failed: {}.  Retrying in 10 seconds...", e);
                    time::sleep(Duration::from_secs(10)).await;
                } else {
                    warn!("Connection failed the maximum number of times: {}.  Will remove pairing and retry...", e);
                    adapter
                        .remove_device(device.address())
                        .await
                        .context("Failed to remove pairing")?;
                    return Err(anyhow!(
                        "Failed to connect to the device after {} attempts.",
                        max_retries
                    ));
                }
            }
        }
    }

    Ok(device)
}

// Find the device characteristics using the provided UUID's
async fn find_characteristics(device: &Device) -> Result<(Characteristic, Characteristic)> {
    let services = device.services().await?;
    let mut notify_char_opt: Option<Characteristic> = None;
    let mut write_char_opt: Option<Characteristic> = None;

    for service in services {
        let characteristics = service.characteristics().await?;
        for characteristic in characteristics {
            let uuid = characteristic.uuid().await?;
            if uuid == WICAN_NOTIFY_UUID {
                notify_char_opt = Some(characteristic);
            } else if uuid == WICAN_WRITE_UUID {
                write_char_opt = Some(characteristic);
            }
        }
    }

    let notify_char = notify_char_opt
        .ok_or_else(|| anyhow!("Could not find the WiCAN notify characteristic."))?;
    let write_char =
        write_char_opt.ok_or_else(|| anyhow!("Could not find the WiCAN write characteristic."))?;

    Ok((notify_char, write_char))
}

// Submit autopid request and parse as JSON
async fn fetch_data(
    device: &Device,
    vehicle_battery_capacity: u32,
    wican_timeout: Duration,
) -> Result<Option<BatteryData>> {
    let (notify_char, write_char) = find_characteristics(device)
        .await
        .context("Failed to find WiCAN characteristics")?;

    let mut notif_stream = Box::pin(notify_char.notify().await?);
    write_char.write(b"autopid -d\n").await?;

    info!(
        "Successfully sent WiCAN autopid request. Waiting for a response for up to 10 seconds..."
    );

    let timeout = time::sleep(wican_timeout);
    tokio::select! {
        _ = timeout => {
            warn!("Timeout: No reply from WiCAN received.");
            Ok(None)
        }
        notification = notif_stream.next() => {
            if let Some(n) = notification {
                let response_string = String::from_utf8(n)
                    .context("Failed to decode WiCAN response as string")?
                    .trim_end()
                    .to_string();

                debug!("Successfully decoded WiCAN response as string: {}", response_string);

                let wican_response: WicanResponse = serde_json::from_str(&response_string)
                    .context("Failed to parse WiCAN response JSON")?;

                debug!("Successfully decoded WiCAN response as JSON: {:?}", wican_response);

                let battery_data = BatteryData {
                    battery_level_percentage: Some(
                        wican_response
                            .soc_d
                            .unwrap_or(wican_response.soc)
                    ),
                    external_temp_celsius: wican_response.outdoor_temperature,
                    battery_capacity_wh: Some(vehicle_battery_capacity),
                    ..Default::default()
                };

                Ok(Some(battery_data))
            } else {
                warn!("Notification stream ended unexpectedly.");
                Ok(None)
            }
        }
    }
}

// Post battery data to aa-proxy-rs
async fn post_battery_data(url: &str, data: &BatteryData) -> Result<()> {
    info!("Sending {:?} to aa-proxy-rs at: {}", data, url);

    let client = Client::new();

    let res = client.post(url).json(data).send().await?;

    if res.status().is_success() {
        info!(
            "Successfully posted to aa-proxy-rs at: {}. Status: {}",
            url,
            res.status()
        );
        Ok(())
    } else {
        let status = res.status();
        warn!(
            "Failed to post to aa-proxy-rs at: {}. Status: {}",
            url, status
        );
        Err(anyhow!(
            "Failed to post to aa-proxy-rs at: {}. Status: {}",
            url,
            status
        ))
    }
}
