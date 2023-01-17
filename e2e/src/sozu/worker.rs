use std::{
    net::SocketAddr,
    os::unix::prelude::{AsRawFd, FromRawFd, IntoRawFd},
    thread::{self, JoinHandle},
};

use mio::net::UnixStream;

use sozu_command_lib as sozu_command;
use sozu_lib as sozu;

use sozu::server::Server;
use sozu_command::{
    channel::Channel,
    config::{Config, FileConfig},
    proxy::{
        Backend, Cluster, HttpFrontend, HttpListenerConfig, HttpsListenerConfig, LoadBalancingAlgorithms,
        LoadBalancingParams, PathRule, ProxyRequest, ProxyRequestOrder, ProxyResponse, Route,
        RulePosition, TcpFrontend, TcpListenerConfig,
    },
    scm_socket::{Listeners, ScmSocket},
    state::ConfigState,
};

use crate::sozu::command_id::CommandID;

/// Handle to a detached thread where a Sozu worker runs
pub struct Worker {
    pub name: String,
    pub config: Config,
    pub state: ConfigState,
    pub scm_main_to_worker: ScmSocket,
    pub scm_worker_to_main: ScmSocket,
    pub command_channel: Channel<ProxyRequest, ProxyResponse>,
    pub command_id: CommandID,
    pub server_job: JoinHandle<()>,
}

/// Used to remove the CLOEXEC flag of socket
/// this allows the socket to live even when its parent process is replaced
pub fn set_no_close_exec(fd: i32) {
    unsafe {
        let old_flags = libc::fcntl(fd, libc::F_GETFD);
        let new_flags = old_flags & !1;
        println!("flags: {} -> {}", old_flags, new_flags);
        libc::fcntl(fd, libc::F_SETFD, new_flags);
    }
}

impl Worker {
    pub fn empty_file_config() -> FileConfig {
        FileConfig {
            command_socket: None,
            saved_state: None,
            automatic_state_save: None,
            worker_count: None,
            worker_automatic_restart: Some(false),
            handle_process_affinity: None,
            command_buffer_size: None,
            max_command_buffer_size: None,
            max_connections: None,
            min_buffers: None,
            max_buffers: None,
            buffer_size: None,
            log_level: None,
            log_target: None,
            log_access_target: None,
            metrics: None,
            listeners: None,
            clusters: None,
            ctl_command_timeout: None,
            pid_file_path: None,
            activate_listeners: None,
            request_timeout: None,
            front_timeout: None,
            back_timeout: None,
            connect_timeout: None,
            zombie_check_interval: None,
            accept_queue_timeout: None,
        }
    }

    pub fn empty_listeners() -> Listeners {
        Listeners {
            http: Vec::new(),
            tls: Vec::new(),
            tcp: Vec::new(),
        }
    }

    pub fn into_config(config: FileConfig) -> Config {
        config.into("").expect("could not create Config")
    }

    pub fn empty_config() -> (Config, Listeners, ConfigState) {
        let listeners = Worker::empty_listeners();
        let config = Worker::empty_file_config();
        let config = Worker::into_config(config);
        let state = ConfigState::new();
        (config, listeners, state)
    }

    pub fn create_server(
        config: Config,
        listeners: Listeners,
        state: ConfigState,
    ) -> (ScmSocket, Channel<ProxyRequest, ProxyResponse>, Server) {
        let (scm_main_to_worker, scm_worker_to_main) =
            UnixStream::pair().expect("could not create unix stream pair");
        let (cmd_main_to_worker, cmd_worker_to_main) =
            Channel::generate(config.command_buffer_size, config.max_command_buffer_size)
                .expect("could not create a channel");

        set_no_close_exec(scm_main_to_worker.as_raw_fd());
        set_no_close_exec(scm_worker_to_main.as_raw_fd());

        let scm_main_to_worker = ScmSocket::new(scm_main_to_worker.into_raw_fd())
            .expect("could not create an SCM socket");
        let scm_worker_to_main = ScmSocket::new(scm_worker_to_main.into_raw_fd())
            .expect("could not create an SCM socket");

        scm_main_to_worker
            .send_listeners(&listeners)
            .expect("could not send listeners");

        let server = Server::try_new_from_config(
            cmd_worker_to_main,
            scm_worker_to_main,
            config,
            state,
            false,
        )
        .expect("could not create sozu worker");

        (scm_main_to_worker, cmd_main_to_worker, server)
    }

    pub fn start_new_worker<S: Into<String>>(
        name: S,
        config: Config,
        listeners: &Listeners,
        state: ConfigState,
    ) -> Self {
        let name = name.into();
        let (scm_main_to_worker, scm_worker_to_main) =
            UnixStream::pair().expect("could not create unix stream pair");
        let (cmd_main_to_worker, cmd_worker_to_main) =
            Channel::generate(config.command_buffer_size, config.max_command_buffer_size)
                .expect("could not create a channel");

        set_no_close_exec(scm_main_to_worker.as_raw_fd());
        set_no_close_exec(scm_worker_to_main.as_raw_fd());
        println!("****Socket fd: {}", scm_main_to_worker.as_raw_fd());
        println!("****Socket fd: {}", scm_worker_to_main.as_raw_fd());
        println!("****Socket fd: {}", cmd_main_to_worker.sock.as_raw_fd());
        println!("****Socket fd: {}", cmd_worker_to_main.sock.as_raw_fd());

        let scm_main_to_worker = ScmSocket::new(scm_main_to_worker.into_raw_fd())
            .expect("could not create an SCM socket");
        let scm_worker_to_main = ScmSocket::new(scm_worker_to_main.into_raw_fd())
            .expect("could not create an SCM socket");
        scm_main_to_worker
            .send_listeners(&listeners)
            .expect("could not send listeners");

        let thread_config = config.to_owned();
        let thread_state = state.to_owned();
        let thread_name = name.to_owned();
        let thread_scm_worker_to_main = scm_worker_to_main.to_owned();
        let server_job = thread::spawn(move || {
            let mut server = Server::try_new_from_config(
                cmd_worker_to_main,
                thread_scm_worker_to_main,
                thread_config,
                thread_state,
                false,
            )
            .expect("could not create sozu worker");
            server.run();
            println!("{} STOPPED", thread_name);
        });

        Self {
            name,
            config,
            state,
            scm_main_to_worker,
            scm_worker_to_main,
            command_channel: cmd_main_to_worker,
            command_id: CommandID::new(),
            server_job,
        }
    }

    pub fn upgrade<S: Into<String>>(&mut self, name: S) -> Self {
        self.send_proxy_request(ProxyRequestOrder::ReturnListenSockets);
        self.read_to_last();

        self.scm_main_to_worker
            .set_blocking(true)
            .expect("Could not set scm socket to blocking");
        let listeners = self
            .scm_main_to_worker
            .receive_listeners()
            .expect("receive listeners");
        println!("Listeners from old worker: {:?}", listeners);
        println!("State from old worker: {:?}", self.state);
        self.send_proxy_request(ProxyRequestOrder::SoftStop);

        let mut worker = Worker::start_new_worker(
            name,
            self.config.to_owned(),
            &listeners,
            self.state.to_owned(),
        );
        worker
            .scm_main_to_worker
            .send_listeners(&listeners)
            .expect("send listeners");
        listeners.close();
        worker.command_id.prefix = "ACTIVATE_".to_string();
        for order in self.state.generate_activate_orders() {
            worker.send_proxy_request(order);
        }
        worker.command_id.prefix = "ID_".to_string();
        worker.read_to_last();

        println!("Upgrade successful, new worker ready");
        worker
    }

    pub fn send_proxy_request(&mut self, order: ProxyRequestOrder) {
        //self.state.handle_order(&order);
        self.command_channel
            .write_message(&ProxyRequest {
                id: self.command_id.next(),
                order,
            })
            .expect("Could not write message on command channel");
    }

    pub fn read_proxy_response(&mut self) -> Option<ProxyResponse> {
        let response = self
            .command_channel
            .read_message()
            .expect("Could not read message on command channel");
        println!("{} received: {:?}", self.name, response);
        Some(response)
    }

    pub fn read_to_last(&mut self) {
        loop {
            let response = self.read_proxy_response();
            if response.unwrap().id == self.command_id.last {
                break;
            }
        }
    }

    pub fn wait_for_server_stop(self) -> bool {
        let result = if self.server_job.is_finished() {
            println!("already finished...");
            true
        } else {
            println!("waiting...");
            match self.server_job.join() {
                Ok(_) => {
                    println!("finished!");
                    true
                }
                Err(error) => {
                    println!("could not join: {:#?}", error);
                    false
                }
            }
        };
        unsafe {
            UnixStream::from_raw_fd(self.scm_main_to_worker.fd);
            UnixStream::from_raw_fd(self.scm_worker_to_main.fd);
        }
        result
    }

    pub fn default_tcp_listener(address: SocketAddr) -> TcpListenerConfig {
        TcpListenerConfig {
            address,
            public_address: None,
            expect_proxy: false,
            front_timeout: 60,
            back_timeout: 30,
            connect_timeout: 3,
        }
    }

    pub fn default_http_listener(address: SocketAddr) -> HttpListenerConfig {
        HttpListenerConfig {
            address,
            public_address: None,
            expect_proxy: false,
            front_timeout: 60,
            back_timeout: 30,
            connect_timeout: 3,
            ..HttpListenerConfig::default()
        }
    }

    pub fn default_https_listener(address: SocketAddr) -> HttpsListenerConfig {
        HttpsListenerConfig {
            address,
            public_address: None,
            expect_proxy: false,
            front_timeout: 60,
            back_timeout: 30,
            connect_timeout: 3,
            ..HttpsListenerConfig::default()
        }
    }

    pub fn default_cluster<S: Into<String>>(cluster_id: S) -> Cluster {
        Cluster {
            cluster_id: cluster_id.into(),
            sticky_session: false,
            https_redirect: false,
            proxy_protocol: None,
            load_balancing: LoadBalancingAlgorithms::default(),
            load_metric: None,
            answer_503: None,
        }
    }

    pub fn default_tcp_frontend<S: Into<String>>(
        cluster_id: S,
        address: SocketAddr,
    ) -> TcpFrontend {
        TcpFrontend {
            cluster_id: cluster_id.into(),
            address,
            tags: None,
        }
    }

    pub fn default_http_frontend<S: Into<String>>(
        cluster_id: S,
        address: SocketAddr,
    ) -> HttpFrontend {
        HttpFrontend {
            route: Route::ClusterId(cluster_id.into()),
            address,
            hostname: String::from("localhost"),
            path: PathRule::Prefix(String::from("/")),
            method: None,
            position: RulePosition::Tree,
            tags: None,
        }
    }

    pub fn default_backend<S1: Into<String>, S2: Into<String>>(
        cluster_id: S1,
        backend_id: S2,
        address: SocketAddr,
    ) -> Backend {
        Backend {
            cluster_id: cluster_id.into(),
            backend_id: backend_id.into(),
            address,
            load_balancing_parameters: Some(LoadBalancingParams::default()),
            sticky_id: None,
            backup: None,
        }
    }
}
