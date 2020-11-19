//! NetworkManager is the one-stop-shop of network configuration on Linux.
use crate::tunnel::wireguard::{
    config::Config as WireguardConfig, wireguard_kernel::MULLVAD_INTERFACE_NAME,
};
use dbus::{
    arg::{self, RefArg, Variant},
    blocking::{stdintf::org_freedesktop_dbus::Properties, Proxy, SyncConnection},
    message::MatchRule,
};
use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader},
    net::IpAddr,
    path::Path,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const NM_BUS: &str = "org.freedesktop.NetworkManager";
const NM_MANAGER: &str = "org.freedesktop.NetworkManager";
const NM_MANAGER_PATH: &str = "/org/freedesktop/NetworkManager";
const CONNECTIVITY_CHECK_KEY: &str = "ConnectivityCheckEnabled";

const NM_DNS_MANAGER: &str = "org.freedesktop.NetworkManager.DnsManager";
const NM_DNS_MANAGER_PATH: &str = "/org/freedesktop/NetworkManager/DnsManager";
const NM_DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const NM_DEVICE_STATISTICS: &str = "org.freedesktop.NetworkManager.Device.Statistics";

const NM_IP4_CONFIG: &str = "org.freedesktop.NetworkManager.IP4Config";
const NM_IP6_CONFIG: &str = "org.freedesktop.NetworkManager.IP6Config";
const DEVICE_READY_TIMEOUT: Duration = Duration::from_secs(15);
const RC_MANAGEMENT_MODE_KEY: &str = "RcManager";
const DNS_MODE_KEY: &str = "Mode";
const DNS_FIRST_PRIORITY: i32 = -2147483647;

const NM_DEVICE_STATE_IP_CHECK: u32 = 80;
const NM_DEVICE_STATE_SECONDARY: u32 = 90;
const NM_DEVICE_STATE_ACTIVATED: u32 = 100;

const NM_SETTINGS_INTERFACE: &str = "org.freedesktop.NetworkManager.Settings";
const NM_SETTINGS_CONNECTION_INTERFACE: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const NM_SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const NM_CONNECTION_ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";

const NM_ADD_CONNECTION_VOLATILE: u32 = 0x2;

const RPC_TIMEOUT: std::time::Duration = Duration::from_secs(3);

const DBUS_UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";

const MINIMUM_SUPPORTED_MAJOR_VERSION: u32 = 1;
const MINIMUM_SUPPORTED_MINOR_VERSION: u32 = 16;


const NM_DEVICE_STATE_CHANGED: &'static str = "StateChanged";

pub type Result<T> = std::result::Result<T, Error>;

#[derive(err_derive::Error, Debug)]
pub enum Error {
    #[error(display = "Error while communicating over Dbus")]
    Dbus(#[error(source)] dbus::Error),

    #[error(display = "Failed to match the returned D-Bus object with expected type")]
    MatchDBusTypeError(#[error(source)] dbus::arg::TypeMismatchError),

    #[error(display = "Configuration has no device associated to it")]
    NoDevice,

    #[error(display = "NetworkManager is too old - {}", _0)]
    NMTooOld(String),

    #[error(display = "Device inactive: {}", _0)]
    DeviceNotReady(u32),

    #[error(display = "Device not found")]
    DeviceNotFound,

    #[error(display = "NetworkManager not detected")]
    NetworkManagerNotDetected,

    #[error(display = "NetworkManager is too old: {}", 0)]
    TooOldNetworkManager(String),

    #[error(display = "NetworkManager is not managing DNS")]
    NetworkManagerNotManagingDns,

    #[error(display = "Failed to get devices from NetworkManager object")]
    ObtainDevices,
}


pub type VariantRefArg = Variant<Box<dyn RefArg>>;
pub type VariantMap = HashMap<String, VariantRefArg>;
// settings are a{sa{sv}}
pub type DeviceConfig = HashMap<String, VariantMap>;

/// Implements functionality to control NetworkManager over DBus.
pub struct NetworkManager {
    connection: Arc<SyncConnection>,
}

impl NetworkManager {
    pub fn new() -> Result<Self> {
        Ok(Self {
            connection: crate::linux::dbus::get_connection()?,
        })
    }

    pub fn create_wg_tunnel(&self, config: &WireguardConfig) -> Result<WireguardTunnel> {
        self.ensure_nm_is_new_enough_for_wireguard()?;
        let tunnel = self.create_wg_tunnel_inner(config)?;
        if let Err(err) = self.wait_until_device_is_ready(&tunnel.device_path) {
            if let Err(removal_error) = self.remove_tunnel(tunnel) {
                log::error!(
                    "Failed to remove WireGuard tunnel after it not becoming ready fast enough: {}",
                    removal_error
                );
            }
            return Err(err);
        }

        Ok(tunnel)
    }

    pub fn get_interface_name(&self, tunnel: &WireguardTunnel) -> Result<String> {
        tunnel
            .device_proxy(&*self.connection)
            .get(NM_DEVICE, "Interface")
            .map_err(Error::Dbus)
    }

    pub fn get_device_state(&self, device: &dbus::Path<'_>) -> Result<u32> {
        self.as_path(device)
            .get(NM_DEVICE, "State")
            .map_err(Error::Dbus)
    }

    fn create_wg_tunnel_inner(&self, config: &WireguardConfig) -> Result<WireguardTunnel> {
        let settings_map = Self::convert_config_to_dbus(config);

        let config_path: dbus::Path<'static> = match self.add_connection_2(&settings_map) {
            Ok((path, _result)) => path,
            Err(Error::Dbus(dbus_error)) if dbus_error.name() == Some(DBUS_UNKNOWN_METHOD) => {
                self.add_connection_unsaved(&settings_map)?.0
            }
            Err(err) => {
                log::error!(
                    "Failed to create a new interface via AddConnection2 - {}",
                    err
                );
                return Err(err);
            }
        };

        let manager = self.nm_manager();
        let (connection_path,): (dbus::Path<'static>,) = manager
            .method_call(
                NM_MANAGER,
                "ActivateConnection",
                (
                    &config_path,
                    &dbus::Path::new("/").unwrap(),
                    &dbus::Path::new("/").unwrap(),
                ),
            )
            .map_err(Error::Dbus)?;

        let connection = Proxy::new(NM_BUS, &connection_path, RPC_TIMEOUT, &*self.connection);
        let device_paths: Vec<dbus::Path<'static>> = connection
            .get(NM_CONNECTION_ACTIVE, "Devices")
            .map_err(Error::Dbus)?;
        let device_path = device_paths.into_iter().next().ok_or(Error::NoDevice)?;


        Ok(WireguardTunnel {
            config_path,
            connection_path,
            device_path,
        })
    }

    fn ensure_nm_is_new_enough_for_wireguard(&self) -> Result<()> {
        let manager = self.nm_manager();
        let version_string: String = manager.get(NM_MANAGER, "Version").map_err(Error::Dbus)?;
        let version_too_old = || Error::NMTooOld(version_string.clone());
        let mut parts = version_string
            .split(".")
            .map(|part| part.parse().map_err(|_| version_too_old()));

        let major_version: u32 = parts.next().ok_or_else(|| version_too_old())??;
        let minor_version: u32 = parts.next().ok_or_else(|| version_too_old())??;

        if major_version < MINIMUM_SUPPORTED_MAJOR_VERSION
            || minor_version < MINIMUM_SUPPORTED_MINOR_VERSION
        {
            Err(version_too_old())
        } else {
            Ok(())
        }
    }

    fn add_connection_2(
        &self,
        settings_map: &DeviceConfig,
    ) -> Result<(dbus::Path<'static>, DeviceConfig)> {
        let args: VariantMap = HashMap::new();

        Proxy::new(NM_BUS, NM_SETTINGS_PATH, RPC_TIMEOUT, &*self.connection)
            .method_call(
                NM_SETTINGS_INTERFACE,
                "AddConnection2",
                (settings_map, NM_ADD_CONNECTION_VOLATILE, args),
            )
            .map_err(Error::Dbus)
    }

    fn add_connection_unsaved(
        &self,
        settings_map: &DeviceConfig,
    ) -> Result<(dbus::Path<'static>,)> {
        Proxy::new(NM_BUS, NM_SETTINGS_PATH, RPC_TIMEOUT, &*self.connection)
            .method_call(
                NM_SETTINGS_INTERFACE,
                "AddConnectionUnsaved",
                (settings_map,),
            )
            .map_err(Error::Dbus)
    }

    pub fn set_stats_refresh_rate(
        &self,
        tunnel: &WireguardTunnel,
        refresh_rate: u32,
    ) -> Result<()> {
        tunnel
            .device_proxy(&*self.connection)
            .set(NM_DEVICE_STATISTICS, "RefreshRateMs", refresh_rate)
            .map_err(Error::Dbus)
    }

    pub fn get_tunnel_stats(&self, tunnel: &WireguardTunnel) -> Result<(u64, u64)> {
        let device = tunnel.device_proxy(&*self.connection);
        let tx_bytes = device.get(NM_DEVICE_STATISTICS, "TxBytes")?;
        let rx_bytes = device.get(NM_DEVICE_STATISTICS, "RxBytes")?;

        Ok((tx_bytes, rx_bytes))
    }

    fn wait_until_device_is_ready(&self, device: &dbus::Path<'_>) -> Result<()> {
        let device_state = self.get_device_state(device)?;

        if !device_is_ready(device_state) {
            let deadline = Instant::now() + DEVICE_READY_TIMEOUT;

            let mut match_rule = MatchRule::new_signal(NM_DEVICE, NM_DEVICE_STATE_CHANGED);

            match_rule.path = Some(device.clone().into_static());
            let device_state = Arc::new(AtomicU32::new(device_state));

            {
                let shared_device_state = device_state.clone();
                let device_matcher = self
                    .connection
                    .add_match(
                        match_rule,
                        move |state_change: DeviceStateChange, _connection, _message| {
                            log::debug!("Received new tunnel state change: {:?}", state_change);
                            let new_state = state_change.new_state;
                            shared_device_state.store(new_state, Ordering::Release);
                            true
                        },
                    )
                    .map_err(Error::Dbus)?;
                while Instant::now() < deadline
                    && !device_is_ready(device_state.load(Ordering::Acquire))
                {
                    if let Err(err) = self.connection.process(RPC_TIMEOUT) {
                        log::error!(
                            "DBus connection failed while waiting for device to be ready: {}",
                            err
                        );
                    }
                }

                if let Err(err) = self.connection.remove_match(device_matcher) {
                    log::error!("Failed to remove match from DBus connection: {}", err);
                }
            }

            let final_device_state = device_state.load(Ordering::Acquire);
            if !device_is_ready(final_device_state) {
                return Err(Error::DeviceNotReady(final_device_state));
            }
        }
        Ok(())
    }

    pub fn remove_tunnel(&self, tunnel: WireguardTunnel) -> Result<()> {
        let deactivation_result: Result<()> = self
            .nm_manager()
            .method_call(
                NM_MANAGER,
                "DeactivateConnection",
                (&tunnel.connection_path,),
            )
            .map_err(Error::Dbus);

        let config_result: Result<()> = tunnel
            .config_proxy(&*self.connection)
            .method_call(NM_SETTINGS_CONNECTION_INTERFACE, "Delete", ())
            .map_err(Error::Dbus);
        deactivation_result?;
        config_result?;
        Ok(())
    }

    /// Ensures NetworkManager's connectivity check is disabled and returns the connectivity check
    /// previous state. Returns true only if the connectivity check was enabled and is now
    /// disabled. Disabling the connectivity check should be done before a firewall is applied
    /// due to the fact that blocking DNS requests can make it hang:
    /// https://gitlab.freedesktop.org/NetworkManager/NetworkManager/-/issues/404
    pub fn disable_connectivity_check(&self) -> Option<bool> {
        let nm_manager = self.nm_manager();
        match nm_manager.get(NM_MANAGER, CONNECTIVITY_CHECK_KEY) {
            Ok(true) => {
                if let Err(err) = nm_manager.set(NM_MANAGER, CONNECTIVITY_CHECK_KEY, false) {
                    log::error!(
                        "Failed to disable NetworkManager connectivity check: {}",
                        err
                    );
                    Some(false)
                } else {
                    Some(true)
                }
            }
            Ok(false) => Some(false),
            Err(_) => None,
        }
    }

    /// Enabled NetworkManager's connectivity check. Fails silently.
    pub fn enable_connectivity_check(&self) {
        if let Err(err) = self
            .nm_manager()
            .set(NM_MANAGER, CONNECTIVITY_CHECK_KEY, true)
        {
            log::error!("Failed to reset NetworkManager connectivity check: {}", err);
        }
    }

    fn nm_manager(&self) -> Proxy<'_, &SyncConnection> {
        Proxy::new(NM_BUS, NM_MANAGER_PATH, RPC_TIMEOUT, &*self.connection)
    }

    fn convert_config_to_dbus(config: &WireguardConfig) -> DeviceConfig {
        let mut ipv6_config: VariantMap = HashMap::new();
        let mut ipv4_config: VariantMap = HashMap::new();
        let mut wireguard_config: VariantMap = HashMap::new();
        let mut connection_config: VariantMap = HashMap::new();
        let mut peer_configs = vec![];

        wireguard_config.insert("mtu".into(), Variant(Box::new(config.mtu as u32)));
        wireguard_config.insert("fwmark".into(), Variant(Box::new(config.fwmark as u32)));
        wireguard_config.insert("peer-routes".into(), Variant(Box::new(false)));
        wireguard_config.insert(
            "private-key".into(),
            Variant(Box::new(config.tunnel.private_key.to_base64())),
        );
        wireguard_config.insert("private-key-flags".into(), Variant(Box::new(0x0u32)));

        for peer in config.peers.iter() {
            let mut peer_config: VariantMap = HashMap::new();
            let allowed_ips = peer
                .allowed_ips
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();


            peer_config.insert("allowed-ips".into(), Variant(Box::new(allowed_ips)));
            peer_config.insert(
                "endpoint".into(),
                Variant(Box::new(peer.endpoint.to_string())),
            );
            peer_config.insert(
                "public-key".into(),
                Variant(Box::new(peer.public_key.to_base64())),
            );

            peer_configs.push(peer_config);
        }
        wireguard_config.insert("peers".into(), Variant(Box::new(peer_configs)));

        connection_config.insert("type".into(), Variant(Box::new("wireguard".to_string())));
        connection_config.insert(
            "id".into(),
            Variant(Box::new(MULLVAD_INTERFACE_NAME.to_string())),
        );
        connection_config.insert(
            "interface-name".into(),
            Variant(Box::new(MULLVAD_INTERFACE_NAME.to_string())),
        );
        connection_config.insert("autoconnect".into(), Variant(Box::new(true)));


        let ipv4_addrs: Vec<_> = config
            .tunnel
            .addresses
            .iter()
            .filter(|ip| ip.is_ipv4())
            .map(Self::convert_address_to_dbus)
            .collect();

        let ipv6_addrs: Vec<_> = config
            .tunnel
            .addresses
            .iter()
            .filter(|ip| ip.is_ipv6())
            .map(Self::convert_address_to_dbus)
            .collect();

        ipv4_config.insert("address-data".into(), Variant(Box::new(ipv4_addrs)));
        ipv4_config.insert("ignore-auto-routes".into(), Variant(Box::new(true)));
        ipv4_config.insert("ignore-auto-dns".into(), Variant(Box::new(true)));
        ipv4_config.insert("may-fail".into(), Variant(Box::new(true)));
        ipv4_config.insert("method".into(), Variant(Box::new("manual".to_string())));
        ipv4_config.insert("never-default".into(), Variant(Box::new(true)));

        if !ipv6_addrs.is_empty() {
            ipv6_config.insert("method".into(), Variant(Box::new("manual".to_string())));
            ipv6_config.insert("address-data".into(), Variant(Box::new(ipv6_addrs)));
            ipv6_config.insert("ignore-auto-routes".into(), Variant(Box::new(true)));
            ipv6_config.insert("ignore-auto-dns".into(), Variant(Box::new(true)));
            ipv6_config.insert("may-fail".into(), Variant(Box::new(true)));
        }


        let mut settings = HashMap::new();
        settings.insert("ipv4".into(), ipv4_config);
        if !ipv6_config.is_empty() {
            settings.insert("ipv6".into(), ipv6_config);
        }
        settings.insert("wireguard".into(), wireguard_config);
        settings.insert("connection".into(), connection_config);

        settings
    }

    fn convert_address_to_dbus(address: &IpAddr) -> VariantMap {
        let mut map: VariantMap = HashMap::new();
        map.insert(
            "address".to_string(),
            Variant(Box::new(address.to_string())),
        );
        let prefix: u32 = if address.is_ipv4() { 32 } else { 128 };
        map.insert("prefix".to_string(), Variant(Box::new(prefix)));

        map
    }

    pub fn ensure_network_manager_exists(&self) -> Result<()> {
        match self
            .as_manager()
            .get::<Box<dyn RefArg>>(&NM_MANAGER, "Version")
        {
            Ok(_) => Ok(()),
            Err(err) => {
                log::error!("Failed to read version of NetworkManager {}", err);
                Err(Error::NetworkManagerNotDetected)
            }
        }
    }

    pub fn ensure_resolv_conf_is_managed(&self) -> Result<()> {
        // check if NM is set to manage resolv.conf
        let management_mode: String = self
            .as_dns_manager()
            .get(NM_DNS_MANAGER, RC_MANAGEMENT_MODE_KEY)?;
        if management_mode == "unmanaged" {
            return Err(Error::NetworkManagerNotManagingDns);
        }

        let dns_mode: String = self
            .as_dns_manager()
            .get(NM_DNS_MANAGER, DNS_MODE_KEY)
            .map_err(Error::Dbus)?;

        match dns_mode.as_ref() {
            // If NetworkManager isn't managing DNS for us, it's useless.
            "none" => return Err(Error::NetworkManagerNotManagingDns),
            _ => (),
        };

        if !verify_etc_resolv_conf_contents() {
            log::debug!("/etc/resolv.conf differs from reference resolv.conf, therefore NM is not managing DNS");
            return Err(Error::NetworkManagerNotManagingDns);
        }

        Ok(())
    }

    fn as_manager<'a>(&'a self) -> Proxy<'a, &SyncConnection> {
        Proxy::new(NM_BUS, NM_MANAGER_PATH, RPC_TIMEOUT, &*self.connection)
    }

    fn as_dns_manager<'a>(&'a self) -> Proxy<'a, &SyncConnection> {
        Proxy::new(NM_BUS, NM_DNS_MANAGER_PATH, RPC_TIMEOUT, &*self.connection)
    }

    fn as_path<'a>(&'a self, device: &'a dbus::Path<'a>) -> Proxy<'a, &SyncConnection> {
        Proxy::new(NM_BUS, device, RPC_TIMEOUT, &*self.connection)
    }

    pub fn set_dns(&mut self, interface_name: &str, servers: &[IpAddr]) -> Result<DeviceConfig> {
        let device_path = self.fetch_device(interface_name)?;
        self.wait_until_device_is_ready(&device_path)?;


        let device = self.as_path(&device_path);
        // Get the last applied connection
        let (mut settings, version_id): (
            HashMap<String, HashMap<String, Variant<Box<dyn RefArg>>>>,
            u64,
        ) = device.method_call(NM_DEVICE, "GetAppliedConnection", (0u32,))?;

        // Keep changed routes.
        // These routes were modified outside NM, likely by RouteManager.
        if let Some(ipv4_settings) = settings.get_mut("ipv4") {
            let device_ip4_config: dbus::Path<'_> =
                device.get(NM_DEVICE, "Ip4Config").map_err(Error::Dbus)?;

            let device_routes: Vec<Vec<u32>> = self
                .as_path(&device_ip4_config)
                .get(NM_IP4_CONFIG, "Routes")
                .map_err(Error::Dbus)?;

            let device_route_data: Vec<HashMap<String, Variant<Box<dyn RefArg>>>> = self
                .as_path(&device_ip4_config)
                .get(NM_IP4_CONFIG, "RouteData")
                .map_err(Error::Dbus)?;

            ipv4_settings.insert("route-metric".to_string(), Variant(Box::new(0u32)));
            ipv4_settings.insert("routes".to_string(), Variant(Box::new(device_routes)));
            ipv4_settings.insert(
                "route-data".to_string(),
                Variant(Box::new(device_route_data)),
            );
        }

        if let Some(ipv6_settings) = settings.get_mut("ipv6") {
            let device_ip6_config: dbus::Path<'_> =
                device.get(NM_DEVICE, "Ip6Config").map_err(Error::Dbus)?;

            let device_addresses6: Vec<(Vec<u8>, u32, Vec<u8>)> = self
                .as_path(&device_ip6_config)
                .get(NM_IP6_CONFIG, "Addresses")
                .map_err(Error::Dbus)?;

            let device_routes6: Vec<(Vec<u8>, u32, Vec<u8>, u32)> = self
                .as_path(&device_ip6_config)
                .get(NM_IP6_CONFIG, "Routes")
                .map_err(Error::Dbus)?;

            let device_route6_data: Vec<HashMap<String, Variant<Box<dyn RefArg>>>> = self
                .as_path(&device_ip6_config)
                .get(NM_IP6_CONFIG, "RouteData")
                .map_err(Error::Dbus)?;

            ipv6_settings.insert("route-metric".to_string(), Variant(Box::new(0u32)));
            ipv6_settings.insert("routes".to_string(), Variant(Box::new(device_routes6)));
            ipv6_settings.insert(
                "route-data".to_string(),
                Variant(Box::new(device_route6_data)),
            );
            // if the link contains link local addresses, addresses shouldn't be reset
            if ipv6_settings
                .get("method")
                .map(|method| {
                    // if IPv6 isn't enabled, IPv6 method will be set to "ignore", in which case we
                    // shouldn't reapply any config for ipv6
                    method.as_str() != Some("link-local") && method.as_str() != Some("ignore")
                })
                .unwrap_or(true)
            {
                ipv6_settings.insert(
                    "addresses".to_string(),
                    Variant(Box::new(device_addresses6)),
                );
            }
        }

        let mut settings_backup =
            HashMap::<String, HashMap<String, Variant<Box<dyn RefArg>>>>::new();
        for (top_key, map) in settings.iter() {
            let mut inner_dict = HashMap::<String, Variant<Box<dyn RefArg>>>::new();
            for (key, variant) in map.iter() {
                inner_dict.insert(key.to_string(), Variant(variant.0.box_clone()));
            }
            settings_backup.insert(top_key.to_string(), inner_dict);
        }

        // Update the DNS config
        let v4_dns: Vec<u32> = servers
            .iter()
            .filter_map(|server| {
                match server {
                    // Network-byte order
                    IpAddr::V4(server) => Some(u32::to_be(server.clone().into())),
                    IpAddr::V6(_) => None,
                }
            })
            .collect();
        if !v4_dns.is_empty() {
            Self::update_dns_config(&mut settings, "ipv4", v4_dns);
        }

        let v6_dns: Vec<Vec<u8>> = servers
            .iter()
            .filter_map(|server| match server {
                IpAddr::V4(_) => None,
                IpAddr::V6(server) => Some(server.octets().to_vec()),
            })
            .collect();
        if !v6_dns.is_empty() {
            Self::update_dns_config(&mut settings, "ipv6", v6_dns);
        }

        if let Some(wg_config) = settings.get_mut("wireguard") {
            wg_config.insert(
                "fwmark".to_string(),
                Variant(Box::new(crate::linux::TUNNEL_FW_MARK) as Box<dyn RefArg>),
            );
        }

        self.reapply_settings(&device_path, settings, version_id)?;
        Ok(settings_backup)
    }

    pub fn reapply_settings<Settings: arg::Append>(
        &self,
        device: &dbus::Path<'_>,
        settings: Settings,
        version_id: u64,
    ) -> Result<()> {
        self.as_path(device)
            .method_call(NM_DEVICE, "Reapply", (settings, version_id, 0u32))?;
        Ok(())
    }

    fn update_dns_config<'a, T>(
        settings: &mut HashMap<String, HashMap<String, Variant<Box<dyn RefArg + 'a>>>>,
        ip_protocol: &'static str,
        servers: T,
    ) where
        T: RefArg + 'a,
    {
        let settings = match settings.get_mut(ip_protocol) {
            Some(ip_protocol) => ip_protocol,
            None => {
                settings.insert(ip_protocol.to_string(), HashMap::new());
                settings.get_mut(ip_protocol).unwrap()
            }
        };

        settings.insert(
            "method".to_string(),
            Variant(Box::new("manual".to_string())),
        );
        settings.insert(
            "dns-priority".to_string(),
            Variant(Box::new(DNS_FIRST_PRIORITY)),
        );
        settings.insert("dns".to_string(), Variant(Box::new(servers)));
        settings.insert(
            "dns-search".to_string(),
            Variant(Box::new(vec!["~.".to_string()])),
        );
    }

    pub fn fetch_device(&self, interface_name: &str) -> Result<dbus::Path<'static>> {
        let devices: Box<dyn RefArg> = self
            .as_manager()
            .get(NM_MANAGER, "Devices")
            .map_err(Error::Dbus)?;
        let mut iter = devices
            .as_iter()
            .ok_or(Error::ObtainDevices)?
            .map(|device| device.box_clone());

        while let Some(device_item) = iter.next() {
            // Copy due to lifetime weirdness
            let device_path = device_item
                .as_any()
                .downcast_ref::<dbus::Path<'_>>()
                .ok_or(Error::ObtainDevices)?;

            let device_name: String = self
                .as_path(&device_path)
                .get(NM_DEVICE, "Interface")
                .map_err(Error::Dbus)?;

            if device_name != interface_name {
                continue;
            }

            return Ok(device_path.clone());
        }
        Err(Error::DeviceNotFound)
    }
}

#[derive(Debug)]
struct DeviceStateChange {
    new_state: u32,
    old_state: u32,
    reason: u32,
}

impl arg::ReadAll for DeviceStateChange {
    fn read(i: &mut arg::Iter<'_>) -> std::result::Result<Self, arg::TypeMismatchError> {
        Ok(DeviceStateChange {
            new_state: i.read()?,
            old_state: i.read()?,
            reason: i.read()?,
        })
    }
}

impl dbus::message::SignalArgs for DeviceStateChange {
    const NAME: &'static str = NM_DEVICE_STATE_CHANGED;
    const INTERFACE: &'static str = NM_DEVICE;
}

#[derive(Debug)]
pub struct WireguardTunnel {
    config_path: dbus::Path<'static>,
    connection_path: dbus::Path<'static>,
    device_path: dbus::Path<'static>,
}

impl WireguardTunnel {
    fn device_proxy<'a>(&'a self, connection: &'a SyncConnection) -> Proxy<'a, &SyncConnection> {
        Proxy::new(NM_BUS, &self.device_path, RPC_TIMEOUT, connection)
    }

    fn config_proxy<'a>(&'a self, connection: &'a SyncConnection) -> Proxy<'a, &SyncConnection> {
        Proxy::new(NM_BUS, &self.config_path, RPC_TIMEOUT, connection)
    }
}


pub fn device_is_ready(device_state: u32) -> bool {
    /// Any state above `NM_DEVICE_STATE_IP_CONFIG` is considered to be an OK state to change the
    /// DNS config. For the enums, see https://developer.gnome.org/NetworkManager/stable/nm-dbus-types.html#NMDeviceState
    const READY_STATES: [u32; 3] = [
        NM_DEVICE_STATE_IP_CHECK,
        NM_DEVICE_STATE_SECONDARY,
        NM_DEVICE_STATE_ACTIVATED,
    ];
    READY_STATES.contains(&device_state)
}

// Verify that the contents of /etc/resolv.conf match what NM expectes them to be.
fn verify_etc_resolv_conf_contents() -> bool {
    let expected_resolv_conf = "/var/run/NetworkManager/resolv.conf";
    let actual_resolv_conf = "/etc/resolv.conf";
    eq_file_content(&expected_resolv_conf, &actual_resolv_conf)
}


fn eq_file_content<P: AsRef<Path>>(a: &P, b: &P) -> bool {
    let file_a = match File::open(a).map(BufReader::new) {
        Ok(file) => file,
        Err(e) => {
            log::debug!("Failed top open file {}: {}", a.as_ref().display(), e);
            return false;
        }
    };
    let file_b = match File::open(b).map(BufReader::new) {
        Ok(file) => file,
        Err(e) => {
            log::debug!("Failed top open file {}: {}", b.as_ref().display(), e);
            return false;
        }
    };

    !file_a
        .lines()
        .zip(file_b.lines())
        .any(|(a, b)| match (a, b) {
            (Ok(a), Ok(b)) => a != b,
            _ => false,
        })
}
