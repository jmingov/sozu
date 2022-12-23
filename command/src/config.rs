//! parsing data from the configuration file
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    fs::File,
    io::{self, Error, ErrorKind, Read},
    net::SocketAddr,
    path::PathBuf,
};

use anyhow::{bail, Context};
use toml;
use tracing::{debug, info, error};

use crate::{
    certificate::split_certificate_chain,
    command::{CommandRequest, CommandRequestOrder, PROTOCOL_VERSION},
    proxy::{
        ActivateListener, AddCertificate, Backend, CertificateAndKey, Cluster, HttpFrontend,
        HttpListener, HttpsListener, ListenerType, LoadBalancingAlgorithms, LoadBalancingParams,
        LoadMetric, PathRule, ProxyRequestOrder, Route, RulePosition, TcpFrontend, TcpListener,
        TlsVersion,
    },
};

/// [`DEFAULT_RUSTLS_CIPHER_LIST`] provides all supported cipher suites exported by Rustls TLS
/// provider as it support only strongly secure ones.
///
/// See the [documentation](https://docs.rs/rustls/latest/rustls/static.ALL_CIPHER_SUITES.html)
pub const DEFAULT_RUSTLS_CIPHER_LIST: [&'static str; 9] = [
    // TLS 1.3 cipher suites
    "TLS13_AES_256_GCM_SHA384",
    "TLS13_AES_128_GCM_SHA256",
    "TLS13_CHACHA20_POLY1305_SHA256",
    // TLS 1.2 cipher suites
    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
    "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
    "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
];

pub const DEFAULT_CIPHER_SUITES: [&'static str; 4] = [
    "TLS_AES_256_GCM_SHA384",
    "TLS_AES_128_GCM_SHA256",
    "TLS_AES_128_CCM_SHA256",
    "TLS_CHACHA20_POLY1305_SHA256",
];

pub const DEFAULT_SIGNATURE_ALGORITHMS: [&'static str; 9] = [
    "ECDSA+SHA256",
    "ECDSA+SHA384",
    "ECDSA+SHA512",
    "RSA+SHA256",
    "RSA+SHA384",
    "RSA+SHA512",
    "RSA-PSS+SHA256",
    "RSA-PSS+SHA384",
    "RSA-PSS+SHA512",
];

pub const DEFAULT_GROUPS_LIST: [&'static str; 4] = ["P-521", "P-384", "P-256", "x25519"];

// todo: refactor this with a builder pattern for cleanliness
/// an HTTP, HTTPS or TCP listener as ordered by a client
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    pub address: SocketAddr,
    pub protocol: FileListenerProtocolConfig,
    pub public_address: Option<SocketAddr>,
    pub answer_404: Option<String>,
    pub answer_503: Option<String>,
    pub tls_versions: Option<Vec<TlsVersion>>,
    pub cipher_list: Option<Vec<String>>,
    pub expect_proxy: Option<bool>,
    #[serde(default = "default_sticky_name")]
    pub sticky_name: String,
    pub certificate: Option<String>,
    pub certificate_chain: Option<String>,
    pub key: Option<String>,
    pub front_timeout: Option<u32>,
    pub back_timeout: Option<u32>,
    pub connect_timeout: Option<u32>,
    pub request_timeout: Option<u32>,
}

fn default_sticky_name() -> String {
    String::from("SOZUBALANCEID")
}

impl Listener {
    pub fn new(address: SocketAddr, protocol: FileListenerProtocolConfig) -> Listener {
        Listener {
            address,
            protocol,
            public_address: None,
            answer_404: None,
            answer_503: None,
            tls_versions: None,
            cipher_list: None,
            expect_proxy: None,
            sticky_name: String::from("SOZUBALANCEID"),
            certificate: None,
            certificate_chain: None,
            key: None,
            front_timeout: None,
            back_timeout: None,
            connect_timeout: None,
            request_timeout: None,
        }
    }

    pub fn to_http(
        &self,
        front_timeout: Option<u32>,
        back_timeout: Option<u32>,
        connect_timeout: Option<u32>,
        request_timeout: Option<u32>,
    ) -> anyhow::Result<HttpListener> {
        if self.protocol != FileListenerProtocolConfig::Http {
            bail!("cannot convert listener to HTTP");
        }

        /*FIXME
        let mut address = self.address.clone();
        address.push(':');
        address.push_str(&self.port.to_string());

        let http_proxy_configuration = match address.parse() {
          Ok(addr) => Some(addr),
          Err(err) => {
            error!("Couldn't parse address of HTTP proxy: {}", err);
            None
          }
        };
        */
        // let http_proxy_configuration = Some(self.address);

        let mut configuration = HttpListener {
            address: self.address,
            public_address: self.public_address,
            expect_proxy: self.expect_proxy.unwrap_or(false),
            sticky_name: self.sticky_name.clone(),
            front_timeout: self.front_timeout.or(front_timeout).unwrap_or(60),
            back_timeout: self.back_timeout.or(back_timeout).unwrap_or(30),
            connect_timeout: self.connect_timeout.or(connect_timeout).unwrap_or(3),
            request_timeout: self.request_timeout.or(request_timeout).unwrap_or(10),
            ..Default::default()
        };

        let (answer_404, answer_503) = self
            .get_404_503_answers()
            .with_context(|| "Could not get 404 and 503 answers from file system")?;
        configuration.answer_404 = answer_404;
        configuration.answer_503 = answer_503;

        Ok(configuration)
    }

    pub fn to_tls(
        &self,
        front_timeout: Option<u32>,
        back_timeout: Option<u32>,
        connect_timeout: Option<u32>,
        request_timeout: Option<u32>,
    ) -> anyhow::Result<HttpsListener> {
        if self.protocol != FileListenerProtocolConfig::Https {
            bail!("cannot convert listener to HTTPS");
        }

        let default_cipher_list = DEFAULT_RUSTLS_CIPHER_LIST
            .into_iter()
            .map(String::from)
            .collect();

        let cipher_list = self.cipher_list.clone().unwrap_or(default_cipher_list);

        //FIXME => done. This seems useless now
        // let tls_proxy_configuration = Some(self.address);

        let versions = match self.tls_versions {
            None => vec![TlsVersion::TLSv1_2, TlsVersion::TLSv1_3],
            Some(ref v) => v.clone(),
        };

        let key = self.key.as_ref().and_then(|path| {
            Config::load_file(path)
                .map_err(|e| {
                    error!("cannot load key at path '{}': {:?}", path, e);
                    e
                })
                .ok()
        });
        let certificate = self.certificate.as_ref().and_then(|path| {
            Config::load_file(path)
                .map_err(|e| {
                    error!("cannot load certificate at path '{}': {:?}", path, e);
                    e
                })
                .ok()
        });
        let certificate_chain = self
            .certificate_chain
            .as_ref()
            .and_then(|path| {
                Config::load_file(path)
                    .map_err(|e| {
                        error!("cannot load certificate chain at path '{}': {:?}", path, e);
                        e
                    })
                    .ok()
            })
            .map(split_certificate_chain)
            .unwrap_or_else(Vec::new);

        let expect_proxy = self.expect_proxy.unwrap_or(false);

        let mut configuration = HttpsListener {
            address: self.address,
            sticky_name: self.sticky_name.clone(),
            public_address: self.public_address,
            cipher_list,
            versions,
            expect_proxy,
            key,
            certificate,
            certificate_chain,
            front_timeout: self.front_timeout.or(front_timeout).unwrap_or(60),
            back_timeout: self.back_timeout.or(back_timeout).unwrap_or(30),
            connect_timeout: self.connect_timeout.or(connect_timeout).unwrap_or(3),
            request_timeout: self.request_timeout.or(request_timeout).unwrap_or(10),
            ..Default::default()
        };

        let (answer_404, answer_503) = self
            .get_404_503_answers()
            .with_context(|| "Could not get 404 and 503 answers from file system")?;
        configuration.answer_404 = answer_404;
        configuration.answer_503 = answer_503;

        if let Some(cipher_list) = self.cipher_list.as_ref() {
            configuration.cipher_list = cipher_list.clone();
        }

        Ok(configuration)
    }

    pub fn to_tcp(
        &self,
        front_timeout: Option<u32>,
        back_timeout: Option<u32>,
        connect_timeout: Option<u32>,
    ) -> anyhow::Result<TcpListener> {
        // what does this code do? should we remove it?
        /*let mut address = self.address.clone();
        address.push(':');
        address.push_str(&self.port.to_string());
        */

        /*let addr_parsed = match address.parse() {
          Ok(addr) => Some(addr),
          Err(err) => {
            error!("Couldn't parse address of HTTP proxy: {}", err);
            None
          }
        };

        let addr = addr_parsed;
        */

        Ok(TcpListener {
            address: self.address,
            public_address: self.public_address,
            expect_proxy: self.expect_proxy.unwrap_or(false),
            front_timeout: self.front_timeout.or(front_timeout).unwrap_or(60),
            back_timeout: self.back_timeout.or(back_timeout).unwrap_or(30),
            connect_timeout: self.connect_timeout.or(connect_timeout).unwrap_or(3),
        })
    }

    fn get_404_503_answers(&self) -> anyhow::Result<(String, String)> {
        let answer_404 = match &self.answer_404 {
            Some(a_404_path) => {
                let mut a_404 = String::new();
                let mut file = File::open(a_404_path).with_context(|| {
                    format!(
                        "Could not open 404 answer file on path {}, current dir {:?}",
                        a_404_path,
                        std::env::current_dir().ok()
                    )
                })?;

                file.read_to_string(&mut a_404).with_context(|| {
                    format!(
                        "Could not read 404 answer file on path {}, current dir {:?}",
                        a_404_path,
                        std::env::current_dir().ok()
                    )
                })?;
                a_404
            }
            None => String::from(include_str!("../assets/404.html")),
        };

        let answer_503 = match &self.answer_503 {
            Some(a_503_path) => {
                let mut a_503 = String::new();
                let mut file = File::open(a_503_path).with_context(|| {
                    format!("Could not open 503 answer file on path {}", a_503_path)
                })?;

                file.read_to_string(&mut a_503).with_context(|| {
                    format!("Could not read 503 answer file on path {}", a_503_path)
                })?;
                a_503
            }
            None => String::from(include_str!("../assets/503.html")),
        };
        Ok((answer_404, answer_503))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    pub address: SocketAddr,
    #[serde(default)]
    pub tagged_metrics: bool,
    #[serde(default)]
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[serde(deny_unknown_fields)]
pub enum ProxyProtocolConfig {
    ExpectHeader,
    SendHeader,
    RelayHeader,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[serde(deny_unknown_fields)]
pub enum PathRuleType {
    Prefix,
    Regex,
    Equals,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileClusterFrontendConfig {
    pub address: SocketAddr,
    pub hostname: Option<String>,
    /// creates a path routing rule where the request URL path has to match this
    pub path: Option<String>,
    /// declares whether the path rule is Prefix (default), Regex, or Equals
    pub path_type: Option<PathRuleType>,
    pub method: Option<String>,
    pub certificate: Option<String>,
    pub key: Option<String>,
    pub certificate_chain: Option<String>,
    #[serde(default)]
    pub tls_versions: Vec<TlsVersion>,
    #[serde(default)]
    pub position: RulePosition,
    pub tags: Option<BTreeMap<String, String>>,
}

impl FileClusterFrontendConfig {
    pub fn to_tcp_front(&self) -> anyhow::Result<TcpFrontendConfig> {
        if self.hostname.is_some() {
            bail!("invalid 'hostname' field for TCP frontend");
        }
        if self.path.is_some() {
            bail!("invalid 'path_prefix' field for TCP frontend");
        }
        if self.certificate.is_some() {
            bail!("invalid 'certificate' field for TCP frontend");
        }
        if self.hostname.is_some() {
            bail!("invalid 'key' field for TCP frontend");
        }
        if self.certificate_chain.is_some() {
            bail!("invalid 'certificate_chain' field for TCP frontend",);
        }

        Ok(TcpFrontendConfig {
            address: self.address,
            tags: self.tags.clone(),
        })
    }

    pub fn to_http_front(&self, _cluster_id: &str) -> anyhow::Result<HttpFrontendConfig> {
        let hostname = match &self.hostname {
            Some(hostname) => hostname.to_owned(),
            None => bail!("HTTP frontend should have a 'hostname' field"),
        };

        let key_opt = match self.key.as_ref() {
            None => None,
            Some(path) => {
                let key = Config::load_file(path)
                    .with_context(|| format!("cannot load key at path '{}'", path))?;
                Some(key)
            }
        };

        let certificate_opt = match self.certificate.as_ref() {
            None => None,
            Some(path) => {
                let certificate = Config::load_file(path)
                    .with_context(|| format!("cannot load certificate at path '{}'", path))?;
                Some(certificate)
            }
        };

        let chain_opt = match self.certificate_chain.as_ref() {
            None => None,
            Some(path) => {
                let certificate_chain = Config::load_file(path)
                    .with_context(|| format!("cannot load certificate chain at path {}", path))?;
                Some(split_certificate_chain(certificate_chain))
            }
        };

        let path = match (self.path.as_ref(), self.path_type.as_ref()) {
            (None, _) => PathRule::Prefix("".to_string()),
            (Some(s), Some(PathRuleType::Prefix)) => PathRule::Prefix(s.to_string()),
            (Some(s), Some(PathRuleType::Regex)) => PathRule::Regex(s.to_string()),
            (Some(s), Some(PathRuleType::Equals)) => PathRule::Equals(s.to_string()),
            (Some(s), None) => PathRule::Prefix(s.clone()),
        };

        Ok(HttpFrontendConfig {
            address: self.address,
            hostname,
            certificate: certificate_opt,
            key: key_opt,
            certificate_chain: chain_opt,
            tls_versions: self.tls_versions.clone(),
            position: self.position,
            path,
            method: self.method.clone(),
            tags: self.tags.clone(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub enum FileListenerProtocolConfig {
    Http,
    Https,
    Tcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub enum FileClusterProtocolConfig {
    Http,
    Tcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileClusterConfig {
    pub frontends: Vec<FileClusterFrontendConfig>,
    pub backends: Vec<BackendConfig>,
    pub protocol: FileClusterProtocolConfig,
    pub sticky_session: Option<bool>,
    pub https_redirect: Option<bool>,
    #[serde(default)]
    pub send_proxy: Option<bool>,
    #[serde(default)]
    pub load_balancing: LoadBalancingAlgorithms,
    pub answer_503: Option<String>,
    #[serde(default)]
    pub load_metric: Option<LoadMetric>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    pub address: SocketAddr,
    pub weight: Option<u8>,
    pub sticky_id: Option<String>,
    pub backup: Option<bool>,
    pub backend_id: Option<String>,
}

impl FileClusterConfig {
    pub fn to_cluster_config(
        self,
        cluster_id: &str,
        expect_proxy: &HashSet<SocketAddr>,
    ) -> anyhow::Result<ClusterConfig> {
        match self.protocol {
            FileClusterProtocolConfig::Tcp => {
                let mut has_expect_proxy = None;
                let mut frontends = Vec::new();
                for f in self.frontends {
                    if expect_proxy.contains(&f.address) {
                        match has_expect_proxy {
                            Some(true) => {},
                            Some(false) => bail!(format!(
                                "all the listeners for cluster {} should have the same expect_proxy option",
                                cluster_id
                            )),
                            None => has_expect_proxy = Some(true),
                        }
                    } else {
                        match has_expect_proxy {
                            Some(false) => {},
                            Some(true) => bail!(format!(
                                "all the listeners for cluster {} should have the same expect_proxy option",
                                cluster_id
                            )),
                            None => has_expect_proxy = Some(false),
                        }
                    }
                    let tcp_frontend = f.to_tcp_front()?;
                    frontends.push(tcp_frontend);
                }

                let send_proxy = self.send_proxy.unwrap_or(false);
                let expect_proxy = has_expect_proxy.unwrap_or(false);
                let proxy_protocol = match (send_proxy, expect_proxy) {
                    (true, true) => Some(ProxyProtocolConfig::RelayHeader),
                    (true, false) => Some(ProxyProtocolConfig::SendHeader),
                    (false, true) => Some(ProxyProtocolConfig::ExpectHeader),
                    _ => None,
                };

                Ok(ClusterConfig::Tcp(TcpClusterConfig {
                    cluster_id: cluster_id.to_string(),
                    frontends,
                    backends: self.backends,
                    proxy_protocol,
                    load_balancing: self.load_balancing,
                    load_metric: self.load_metric,
                }))
            }
            FileClusterProtocolConfig::Http => {
                let mut frontends = Vec::new();
                for frontend in self.frontends {
                    let http_frontend = frontend
                        .to_http_front(cluster_id)
                        .with_context(|| "Could not convert frontend config to http frontend")?;
                    frontends.push(http_frontend);
                }

                let answer_503 = self.answer_503.as_ref().and_then(|path| {
                    Config::load_file(path)
                        .map_err(|e| {
                            error!("cannot load 503 error page at path '{}': {:?}", path, e);
                            e
                        })
                        .ok()
                });

                Ok(ClusterConfig::Http(HttpClusterConfig {
                    cluster_id: cluster_id.to_string(),
                    frontends,
                    backends: self.backends,
                    sticky_session: self.sticky_session.unwrap_or(false),
                    https_redirect: self.https_redirect.unwrap_or(false),
                    load_balancing: self.load_balancing,
                    load_metric: self.load_metric,
                    answer_503,
                }))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpFrontendConfig {
    pub address: SocketAddr,
    pub hostname: String,
    pub path: PathRule,
    pub method: Option<String>,
    pub certificate: Option<String>,
    pub key: Option<String>,
    pub certificate_chain: Option<Vec<String>>,
    #[serde(default)]
    pub tls_versions: Vec<TlsVersion>,
    #[serde(default)]
    pub position: RulePosition,
    pub tags: Option<BTreeMap<String, String>>,
}

impl HttpFrontendConfig {
    pub fn generate_orders(&self, cluster_id: &str) -> Vec<ProxyRequestOrder> {
        let mut v = Vec::new();

        if self.key.is_some() && self.certificate.is_some() {
            v.push(ProxyRequestOrder::AddCertificate(AddCertificate {
                address: self.address,
                certificate: CertificateAndKey {
                    key: self.key.clone().unwrap(),
                    certificate: self.certificate.clone().unwrap(),
                    certificate_chain: self.certificate_chain.clone().unwrap_or_default(),
                    versions: self.tls_versions.clone(),
                },
                names: vec![self.hostname.clone()],
                expired_at: None,
            }));

            v.push(ProxyRequestOrder::AddHttpsFrontend(HttpFrontend {
                route: Route::ClusterId(cluster_id.to_string()),
                address: self.address,
                hostname: self.hostname.clone(),
                path: self.path.clone(),
                method: self.method.clone(),
                position: self.position,
                tags: self.tags.clone(),
            }));
        } else {
            //create the front both for HTTP and HTTPS if possible
            v.push(ProxyRequestOrder::AddHttpFrontend(HttpFrontend {
                route: Route::ClusterId(cluster_id.to_string()),
                address: self.address,
                hostname: self.hostname.clone(),
                path: self.path.clone(),
                method: self.method.clone(),
                position: self.position,
                tags: self.tags.clone(),
            }));
        }

        v
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpClusterConfig {
    pub cluster_id: String,
    pub frontends: Vec<HttpFrontendConfig>,
    pub backends: Vec<BackendConfig>,
    pub sticky_session: bool,
    pub https_redirect: bool,
    pub load_balancing: LoadBalancingAlgorithms,
    pub load_metric: Option<LoadMetric>,
    pub answer_503: Option<String>,
}

impl HttpClusterConfig {
    pub fn generate_orders(&self) -> Vec<ProxyRequestOrder> {
        let mut v = vec![ProxyRequestOrder::AddCluster(Cluster {
            cluster_id: self.cluster_id.clone(),
            sticky_session: self.sticky_session,
            https_redirect: self.https_redirect,
            proxy_protocol: None,
            load_balancing: self.load_balancing,
            answer_503: self.answer_503.clone(),
            load_metric: self.load_metric,
        })];

        for frontend in &self.frontends {
            let mut orders = frontend.generate_orders(&self.cluster_id);
            v.append(&mut orders);
        }

        for (backend_count, backend) in self.backends.iter().enumerate() {
            let load_balancing_parameters = Some(LoadBalancingParams {
                weight: backend.weight.unwrap_or(100),
            });

            v.push(ProxyRequestOrder::AddBackend(Backend {
                cluster_id: self.cluster_id.clone(),
                backend_id: backend.backend_id.clone().unwrap_or_else(|| {
                    format!("{}-{}-{}", self.cluster_id, backend_count, backend.address)
                }),
                address: backend.address,
                load_balancing_parameters,
                sticky_id: backend.sticky_id.clone(),
                backup: backend.backup,
            }));
        }

        v
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TcpFrontendConfig {
    pub address: SocketAddr,
    pub tags: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TcpClusterConfig {
    pub cluster_id: String,
    pub frontends: Vec<TcpFrontendConfig>,
    pub backends: Vec<BackendConfig>,
    #[serde(default)]
    pub proxy_protocol: Option<ProxyProtocolConfig>,
    pub load_balancing: LoadBalancingAlgorithms,
    pub load_metric: Option<LoadMetric>,
}

impl TcpClusterConfig {
    pub fn generate_orders(&self) -> Vec<ProxyRequestOrder> {
        let mut v = vec![ProxyRequestOrder::AddCluster(Cluster {
            cluster_id: self.cluster_id.clone(),
            sticky_session: false,
            https_redirect: false,
            proxy_protocol: self.proxy_protocol.clone(),
            load_balancing: self.load_balancing,
            load_metric: self.load_metric,
            answer_503: None,
        })];

        for frontend in &self.frontends {
            v.push(ProxyRequestOrder::AddTcpFrontend(TcpFrontend {
                cluster_id: self.cluster_id.clone(),
                address: frontend.address,
                tags: frontend.tags.clone(),
            }));
        }

        for (backend_count, backend) in self.backends.iter().enumerate() {
            let load_balancing_parameters = Some(LoadBalancingParams {
                weight: backend.weight.unwrap_or(100),
            });

            v.push(ProxyRequestOrder::AddBackend(Backend {
                cluster_id: self.cluster_id.clone(),
                backend_id: backend.backend_id.clone().unwrap_or_else(|| {
                    format!("{}-{}-{}", self.cluster_id, backend_count, backend.address)
                }),
                address: backend.address,
                load_balancing_parameters,
                sticky_id: backend.sticky_id.clone(),
                backup: backend.backup,
            }));
        }

        v
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClusterConfig {
    Http(HttpClusterConfig),
    Tcp(TcpClusterConfig),
}

impl ClusterConfig {
    pub fn generate_orders(&self) -> Vec<ProxyRequestOrder> {
        match *self {
            ClusterConfig::Http(ref http) => http.generate_orders(),
            ClusterConfig::Tcp(ref tcp) => tcp.generate_orders(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileConfig {
    pub command_socket: Option<String>,
    pub command_buffer_size: Option<usize>,
    pub max_command_buffer_size: Option<usize>,
    pub max_connections: Option<usize>,
    pub min_buffers: Option<usize>,
    pub max_buffers: Option<usize>,
    pub buffer_size: Option<usize>,
    pub saved_state: Option<String>,
    #[serde(default)]
    pub automatic_state_save: Option<bool>,
    pub log_level: Option<String>,
    pub log_target: Option<String>,
    #[serde(default)]
    pub log_access_target: Option<String>,
    pub worker_count: Option<u16>,
    pub worker_automatic_restart: Option<bool>,
    pub metrics: Option<MetricsConfig>,
    pub listeners: Option<Vec<Listener>>,
    pub clusters: Option<HashMap<String, FileClusterConfig>>,
    pub handle_process_affinity: Option<bool>,
    pub ctl_command_timeout: Option<u64>,
    pub pid_file_path: Option<String>,
    pub activate_listeners: Option<bool>,
    #[serde(default)]
    pub front_timeout: Option<u32>,
    #[serde(default)]
    pub back_timeout: Option<u32>,
    #[serde(default)]
    pub connect_timeout: Option<u32>,
    #[serde(default)]
    pub zombie_check_interval: Option<u32>,
    #[serde(default)]
    pub accept_queue_timeout: Option<u32>,
    #[serde(default)]
    pub request_timeout: Option<u32>,
}

impl FileConfig {
    pub fn load_from_path(path: &str) -> io::Result<FileConfig> {
        let data = Config::load_file(path)?;

        match toml::from_str(&data) {
            Err(e) => {
                display_toml_error(&data, &e);
                Err(Error::new(
                    ErrorKind::InvalidData,
                    format!("decoding error: {}", e),
                ))
            }
            Ok(config) => {
                let config: FileConfig = config;
                let mut reserved_address: HashSet<SocketAddr> = HashSet::new();

                if let Some(l) = config.listeners.as_ref() {
                    for listener in l.iter() {
                        if reserved_address.contains(&listener.address) {
                            info!(
                                "listening address {:?} is already used in the configuration",
                                listener.address
                            );
                            return Err(Error::new(
                                ErrorKind::InvalidData,
                                format!(
                                    "listening address {:?} is already used in the configuration",
                                    listener.address
                                ),
                            ));
                        } else {
                            reserved_address.insert(listener.address);
                        }
                    }
                }

                //FIXME: verify how clusters and listeners share addresses
                /*
                if let Some(ref clusters) = config.clusters {
                  for (key, cluster) in clusters.iter() {
                    if let (Some(address), Some(port)) = (cluster.ip_address.clone(), cluster.port) {
                      let addr = (address, port);
                      if reserved_address.contains(&addr) {
                        println!("TCP cluster '{}' listening address ( {}:{} ) is already used in the configuration",
                          key, addr.0, addr.1);
                        return Err(Error::new(
                          ErrorKind::InvalidData,
                          format!("TCP cluster '{}' listening address ( {}:{} ) is already used in the configuration",
                            key, addr.0, addr.1)));
                      } else {
                        reserved_address.insert(addr.clone());
                      }
                    }
                  }
                }
                */

                Ok(config)
            }
        }
    }

    // TODO: split into severa functions
    pub fn into(self, config_path: &str) -> anyhow::Result<Config> {
        let mut clusters = HashMap::new();
        let mut http_listeners = Vec::new();
        let mut https_listeners = Vec::new();
        let mut tcp_listeners = Vec::new();
        let mut known_addresses = HashMap::new();
        let mut expect_proxy = HashSet::new();

        if let Some(listeners) = self.listeners {
            for listener in listeners.iter() {
                if known_addresses.contains_key(&listener.address) {
                    bail!(format!(
                        "there's already a listener for address {:?}",
                        listener.address
                    ));
                }

                known_addresses.insert(listener.address, listener.protocol);
                if listener.expect_proxy == Some(true) {
                    expect_proxy.insert(listener.address);
                }

                if listener.public_address.is_some() && listener.expect_proxy == Some(true) {
                    bail!(format!(
                        "the listener on {} has incompatible options: it cannot use the expect proxy protocol and have a public_address field at the same time",
                        &listener.address
                    ));
                }

                match listener.protocol {
                    FileListenerProtocolConfig::Https => {
                        let listener = listener
                            .to_tls(
                                self.front_timeout,
                                self.back_timeout,
                                self.connect_timeout,
                                self.request_timeout,
                            )
                            .with_context(|| "invalid listener")?;
                        https_listeners.push(listener);
                    }
                    FileListenerProtocolConfig::Http => {
                        let listener = listener
                            .to_http(
                                self.front_timeout,
                                self.back_timeout,
                                self.connect_timeout,
                                self.request_timeout,
                            )
                            .with_context(|| "invalid listener")?;
                        http_listeners.push(listener);
                    }
                    FileListenerProtocolConfig::Tcp => {
                        let listener = listener
                            .to_tcp(self.front_timeout, self.back_timeout, self.connect_timeout)
                            .with_context(|| "invalid listener")?;
                        tcp_listeners.push(listener);
                    }
                }
            }
        }

        if let Some(mut file_cluster_configs) = self.clusters {
            for (id, file_cluster_config) in file_cluster_configs.drain() {
                let mut cluster_config = file_cluster_config
                    .to_cluster_config(id.as_str(), &expect_proxy)
                    .with_context(|| {
                        format!("error parsing cluster configuration for cluster {}", id)
                    })?;

                match cluster_config {
                    ClusterConfig::Http(ref mut http) => {
                        for frontend in http.frontends.iter_mut() {
                            match known_addresses.get(&frontend.address) {
                                Some(FileListenerProtocolConfig::Tcp) => {
                                    bail!(
                                        "cannot set up a HTTP or HTTPS frontend on a TCP listener"
                                    );
                                }
                                Some(FileListenerProtocolConfig::Http) => {
                                    if frontend.certificate.is_some() {
                                        bail!("cannot set up a HTTPS frontend on a HTTP listener");
                                    }
                                }
                                Some(FileListenerProtocolConfig::Https) => {
                                    if frontend.certificate.is_none() {
                                        if let Some(https_listener) =
                                            https_listeners.iter().find(|listener| {
                                                listener.address == frontend.address
                                                    && listener.certificate.is_some()
                                            })
                                        {
                                            //println!("using listener certificate for {:}", frontend.address);
                                            frontend.certificate =
                                                https_listener.certificate.clone();
                                            frontend.certificate_chain =
                                                Some(https_listener.certificate_chain.clone());
                                            frontend.key = https_listener.key.clone();
                                        }
                                        if frontend.certificate.is_none() {
                                            info!("known addresses: {:#?}", known_addresses);
                                            info!("frontend: {:#?}", frontend);
                                            bail!(
                                                "cannot set up a HTTP frontend on a HTTPS listener"
                                            );
                                        }
                                    }
                                }
                                None => {
                                    // create a default listener for that front
                                    let file_listener_protocol = if frontend.certificate.is_some() {
                                        let listener = Listener::new(
                                            frontend.address,
                                            FileListenerProtocolConfig::Https,
                                        );
                                        https_listeners.push(
                                            listener
                                                .to_tls(
                                                    self.front_timeout,
                                                    self.back_timeout,
                                                    self.connect_timeout,
                                                    self.request_timeout,
                                                )
                                                .with_context(|| {
                                                    "Cannot convert listener to TLS"
                                                })?,
                                        );

                                        FileListenerProtocolConfig::Https
                                    } else {
                                        let listener = Listener::new(
                                            frontend.address,
                                            FileListenerProtocolConfig::Http,
                                        );
                                        http_listeners.push(
                                            listener
                                                .to_http(
                                                    self.front_timeout,
                                                    self.back_timeout,
                                                    self.connect_timeout,
                                                    self.request_timeout,
                                                )
                                                .with_context(|| {
                                                    "Cannot convert listener to HTTP"
                                                })?,
                                        );

                                        FileListenerProtocolConfig::Http
                                    };
                                    known_addresses
                                        .insert(frontend.address, file_listener_protocol);
                                }
                            }
                        }
                    }
                    ClusterConfig::Tcp(ref tcp) => {
                        //FIXME: verify that different TCP clusters do not request the same address
                        for frontend in &tcp.frontends {
                            match known_addresses.get(&frontend.address) {
                                Some(FileListenerProtocolConfig::Http)
                                | Some(FileListenerProtocolConfig::Https) => {
                                    bail!("cannot set up a TCP frontend on a HTTP listener");
                                }
                                Some(FileListenerProtocolConfig::Tcp) => {}
                                None => {
                                    // create a default listener for that front
                                    let listener = Listener::new(
                                        frontend.address,
                                        FileListenerProtocolConfig::Tcp,
                                    );
                                    tcp_listeners.push(
                                        listener
                                            .to_tcp(
                                                self.front_timeout,
                                                self.back_timeout,
                                                self.connect_timeout,
                                            )
                                            .with_context(|| "Cannot convert listener to TCP")?,
                                    );
                                    known_addresses
                                        .insert(frontend.address, FileListenerProtocolConfig::Tcp);
                                }
                            }
                        }
                    }
                }

                clusters.insert(id, cluster_config);
            }
        }

        let command_socket_path = self.command_socket.unwrap_or({
            let mut path = env::current_dir().with_context(|| "env path not found")?;
            path.push("sozu.sock");
            let verified_path = path
                .to_str()
                .with_context(|| "command socket path not valid")?;
            verified_path.to_owned()
        });

        if let (None, Some(true)) = (&self.saved_state, &self.automatic_state_save) {
            bail!("cannot activate automatic state save if the 'saved_state` option is not set");
        }

        Ok(Config {
            config_path: config_path.to_string(),
            command_socket: command_socket_path,
            command_buffer_size: self.command_buffer_size.unwrap_or(1_000_000),
            max_command_buffer_size: self
                .max_command_buffer_size
                .unwrap_or(self.command_buffer_size.unwrap_or(1_000_000) * 2),
            max_connections: self.max_connections.unwrap_or(10000),
            min_buffers: std::cmp::min(
                self.min_buffers.unwrap_or(1),
                self.max_buffers.unwrap_or(1000),
            ),
            max_buffers: self.max_buffers.unwrap_or(1000),
            buffer_size: self.buffer_size.unwrap_or(16393),
            saved_state: self.saved_state,
            automatic_state_save: self.automatic_state_save.unwrap_or(false),
            log_level: self.log_level.unwrap_or_else(|| String::from("info")),
            log_target: self.log_target.unwrap_or_else(|| String::from("stdout")),
            log_access_target: self.log_access_target,
            worker_count: self.worker_count.unwrap_or(2),
            worker_automatic_restart: self.worker_automatic_restart.unwrap_or(true),
            metrics: self.metrics,
            http_listeners,
            https_listeners,
            tcp_listeners,
            clusters,
            handle_process_affinity: self.handle_process_affinity.unwrap_or(false),
            ctl_command_timeout: self.ctl_command_timeout.unwrap_or(1_000),
            pid_file_path: self.pid_file_path,
            activate_listeners: self.activate_listeners.unwrap_or(true),
            front_timeout: self.front_timeout.unwrap_or(60),
            back_timeout: self.front_timeout.unwrap_or(30),
            connect_timeout: self.front_timeout.unwrap_or(3),
            //defaults to 30mn
            zombie_check_interval: self.zombie_check_interval.unwrap_or(30 * 60),
            accept_queue_timeout: self.accept_queue_timeout.unwrap_or(60),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub config_path: String,
    pub command_socket: String,
    pub command_buffer_size: usize,
    pub max_command_buffer_size: usize,
    pub max_connections: usize,
    pub min_buffers: usize,
    pub max_buffers: usize,
    pub buffer_size: usize,
    pub saved_state: Option<String>,
    #[serde(default)]
    pub automatic_state_save: bool,
    pub log_level: String,
    pub log_target: String,
    #[serde(default)]
    pub log_access_target: Option<String>,
    pub worker_count: u16,
    pub worker_automatic_restart: bool,
    pub metrics: Option<MetricsConfig>,
    pub http_listeners: Vec<HttpListener>,
    pub https_listeners: Vec<HttpsListener>,
    pub tcp_listeners: Vec<TcpListener>,
    pub clusters: HashMap<String, ClusterConfig>,
    pub handle_process_affinity: bool,
    pub ctl_command_timeout: u64,
    pub pid_file_path: Option<String>,
    pub activate_listeners: bool,
    #[serde(default = "default_front_timeout")]
    pub front_timeout: u32,
    #[serde(default = "default_back_timeout")]
    pub back_timeout: u32,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: u32,
    #[serde(default = "default_zombie_check_interval")]
    pub zombie_check_interval: u32,
    #[serde(default = "default_accept_queue_timeout")]
    pub accept_queue_timeout: u32,
}

fn default_front_timeout() -> u32 {
    60
}

fn default_back_timeout() -> u32 {
    30
}

fn default_connect_timeout() -> u32 {
    3
}

//defaults to 30mn
fn default_zombie_check_interval() -> u32 {
    30 * 60
}

fn default_accept_queue_timeout() -> u32 {
    60
}

impl Config {
    pub fn load_from_path(path: &str) -> anyhow::Result<Config> {
        let file_config =
            FileConfig::load_from_path(path).with_context(|| "Could not load the config file")?;

        let mut config = file_config
            .into(path)
            .with_context(|| "Could not parse config from file")?;

        // replace saved_state with a verified path
        config.saved_state = config
            .saved_state_path()
            .with_context(|| "Invalid saved_state in the config. Check your config file")?;

        Ok(config)
    }

    pub fn generate_config_messages(&self) -> Vec<CommandRequest> {
        let mut v = Vec::new();
        let mut count = 0u8;

        for listener in &self.http_listeners {
            v.push(CommandRequest {
                id: format!("CONFIG-{}", count),
                version: PROTOCOL_VERSION,
                worker_id: None,
                order: CommandRequestOrder::Proxy(Box::new(ProxyRequestOrder::AddHttpListener(
                    listener.clone(),
                ))),
            });
            count += 1;
        }

        for listener in &self.https_listeners {
            v.push(CommandRequest {
                id: format!("CONFIG-{}", count),
                version: PROTOCOL_VERSION,
                worker_id: None,
                order: CommandRequestOrder::Proxy(Box::new(ProxyRequestOrder::AddHttpsListener(
                    listener.clone(),
                ))),
            });
            count += 1;
        }

        for listener in &self.tcp_listeners {
            v.push(CommandRequest {
                id: format!("CONFIG-{}", count),
                version: PROTOCOL_VERSION,
                worker_id: None,
                order: CommandRequestOrder::Proxy(Box::new(ProxyRequestOrder::AddTcpListener(
                    listener.clone(),
                ))),
            });
            count += 1;
        }

        for cluster in self.clusters.values() {
            let mut orders = cluster.generate_orders();
            for order in orders.drain(..) {
                v.push(CommandRequest {
                    id: format!("CONFIG-{}", count),
                    version: PROTOCOL_VERSION,
                    worker_id: None,
                    order: CommandRequestOrder::Proxy(Box::new(order)),
                });
                count += 1;
            }
        }

        if self.activate_listeners {
            for listener in &self.http_listeners {
                v.push(CommandRequest {
                    id: format!("CONFIG-{}", count),
                    version: PROTOCOL_VERSION,
                    worker_id: None,
                    order: CommandRequestOrder::Proxy(Box::new(
                        ProxyRequestOrder::ActivateListener(ActivateListener {
                            address: listener.address,
                            proxy: ListenerType::HTTP,
                            from_scm: false,
                        }),
                    )),
                });
                count += 1;
            }

            for listener in &self.https_listeners {
                v.push(CommandRequest {
                    id: format!("CONFIG-{}", count),
                    version: PROTOCOL_VERSION,
                    worker_id: None,
                    order: CommandRequestOrder::Proxy(Box::new(
                        ProxyRequestOrder::ActivateListener(ActivateListener {
                            address: listener.address,
                            proxy: ListenerType::HTTPS,
                            from_scm: false,
                        }),
                    )),
                });
                count += 1;
            }

            for listener in &self.tcp_listeners {
                v.push(CommandRequest {
                    id: format!("CONFIG-{}", count),
                    version: PROTOCOL_VERSION,
                    worker_id: None,
                    order: CommandRequestOrder::Proxy(Box::new(
                        ProxyRequestOrder::ActivateListener(ActivateListener {
                            address: listener.address,
                            proxy: ListenerType::TCP,
                            from_scm: false,
                        }),
                    )),
                });
                count += 1;
            }
        }

        v
    }

    pub fn command_socket_path(&self) -> anyhow::Result<String> {
        let config_path_buf = PathBuf::from(self.config_path.clone());
        let mut config_folder = match config_path_buf.parent() {
            Some(path) => path.to_path_buf(),
            None => bail!("could not get parent folder of configuration file"),
        };

        let socket_path = PathBuf::from(self.command_socket.clone());
        let mut parent = match socket_path.parent() {
            None => config_folder,
            Some(path) => {
                config_folder.push(path);
                config_folder.canonicalize().with_context(|| {
                    format!("could not get command socket folder path: {:?}", path)
                })?
            }
        };

        let path = match socket_path.file_name() {
            None => bail!("could not get command socket file name"),
            Some(f) => {
                parent.push(f);
                parent
            }
        };

        path.to_str()
            .map(|s| s.to_string())
            .with_context(|| "could not parse command socket path")
    }

    fn saved_state_path(&self) -> anyhow::Result<Option<String>> {
        let path = match self.saved_state.as_ref() {
            Some(path) => path,
            None => return Ok(None),
        };

        debug!("saved_stated path in the config: {}", path);

        let config_path_buf = PathBuf::from(self.config_path.clone());
        debug!("Config path buffer: {:?}", config_path_buf);

        let config_folder = config_path_buf
            .parent()
            .with_context(|| "could not get parent folder of configuration file")?;

        debug!("Config folder: {:?}", config_folder);

        let mut saved_state_path_raw = config_folder.to_path_buf();

        saved_state_path_raw.push(path);
        debug!(
            "Looking for saved state on the path {:?}",
            saved_state_path_raw
        );

        saved_state_path_raw.canonicalize().with_context(|| {
            format!(
                "could not get saved state path from config file input {:?}",
                path
            )
        })?;

        let stringified_path = saved_state_path_raw
            .to_str()
            .ok_or_else(|| anyhow::Error::msg("Invalid character format, expected UTF8"))?
            .to_string();

        Ok(Some(stringified_path))
    }

    pub fn load_file(path: &str) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    pub fn load_file_bytes(path: &str) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
}

pub fn display_toml_error(file: &str, error: &toml::de::Error) {
    println!("error parsing the configuration file: {}", error);
    if let Some((line, column)) = error.line_col() {
        let l_span = line.to_string().len();
        println!("{}| {}", line + 1, file.lines().nth(line).unwrap());
        println!("{}^", " ".repeat(l_span + 2 + column));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toml::to_string;

    #[test]
    fn serialize() {
        let http = Listener {
            address: "127.0.0.1:8080".parse().unwrap(),
            protocol: FileListenerProtocolConfig::Http,
            answer_404: Some(String::from("404.html")),
            answer_503: None,
            public_address: None,
            tls_versions: None,
            cipher_list: None,
            expect_proxy: None,
            sticky_name: "SOZUBALANCEID".to_string(),
            certificate: None,
            certificate_chain: None,
            key: None,
            front_timeout: None,
            back_timeout: None,
            connect_timeout: None,
            request_timeout: None,
        };
        println!("http: {:?}", to_string(&http));
        let https = Listener {
            address: "127.0.0.1:8443".parse().unwrap(),
            protocol: FileListenerProtocolConfig::Https,
            answer_404: Some(String::from("404.html")),
            answer_503: None,
            public_address: None,
            tls_versions: None,
            cipher_list: None,
            expect_proxy: None,
            sticky_name: "SOZUBALANCEID".to_string(),
            certificate: None,
            certificate_chain: None,
            key: None,
            front_timeout: None,
            back_timeout: None,
            connect_timeout: None,
            request_timeout: None,
        };
        println!("https: {:?}", to_string(&https));

        let listeners = vec![http, https];
        let config = FileConfig {
            command_socket: Some(String::from("./command_folder/sock")),
            saved_state: None,
            automatic_state_save: None,
            worker_count: Some(2),
            worker_automatic_restart: Some(true),
            handle_process_affinity: None,
            command_buffer_size: None,
            max_connections: Some(500),
            min_buffers: Some(1),
            max_buffers: Some(500),
            buffer_size: Some(16393),
            max_command_buffer_size: None,
            log_level: None,
            log_target: None,
            log_access_target: None,
            metrics: Some(MetricsConfig {
                address: "127.0.0.1:8125".parse().unwrap(),
                tagged_metrics: false,
                prefix: Some(String::from("sozu-metrics")),
            }),
            listeners: Some(listeners),
            clusters: None,
            ctl_command_timeout: None,
            pid_file_path: None,
            activate_listeners: None,
            front_timeout: None,
            back_timeout: None,
            connect_timeout: None,
            zombie_check_interval: None,
            accept_queue_timeout: None,
            request_timeout: None,
        };

        println!("config: {:?}", to_string(&config));
        let encoded = to_string(&config).unwrap();
        println!("conf:\n{}", encoded);
    }

    #[test]
    fn parse() {
        let path = "assets/config.toml";
        let config = Config::load_from_path(path).unwrap_or_else(|load_error| {
            panic!("Cannot load config from path {}: {:?}", path, load_error)
        });
        println!("config: {:#?}", config);
        //panic!();
    }
}
