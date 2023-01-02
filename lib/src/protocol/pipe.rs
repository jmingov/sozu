use std::{cell::RefCell, net::SocketAddr, rc::Rc};

use mio::net::*;
use mio::*;
use rusty_ulid::Ulid;

use crate::{
    pool::Checkout,
    protocol::http::OptionalString,
    socket::{SocketHandler, SocketResult, TransportProtocol},
    sozu_command::ready::Ready,
    timer::TimeoutContainer,
    ListenerHandler, LogDuration, Protocol, {Readiness, SessionMetrics, SessionResult},
};

use super::{http::LogContext, ProtocolResult, SessionState};

#[derive(PartialEq, Eq)]
pub enum SessionStatus {
    Normal,
    DefaultAnswer,
}

#[derive(Copy, Clone, Debug)]
enum ConnectionStatus {
    Normal,
    ReadOpen,
    WriteOpen,
    Closed,
}

pub struct Pipe<Front: SocketHandler, L: ListenerHandler> {
    backend_buffer: Checkout,
    backend_id: Option<String>,
    pub backend_readiness: Readiness,
    backend_status: ConnectionStatus,
    pub backend_timeout: Option<TimeoutContainer>,
    backend_token: Option<Token>,
    backend: Option<TcpStream>,
    cluster_id: Option<String>,
    frontend_buffer: Checkout,
    pub frontend_readiness: Readiness,
    frontend_status: ConnectionStatus,
    pub frontend_timeout: Option<TimeoutContainer>,
    frontend_token: Token,
    frontend: Front,
    listener: Rc<RefCell<L>>,
    log_ctx: String,
    protocol: Protocol,
    request_id: Ulid,
    session_address: Option<SocketAddr>,
    websocket_context: Option<String>,
}

impl<Front: SocketHandler, L: ListenerHandler> Pipe<Front, L> {
    pub fn new(
        frontend: Front,
        frontend_token: Token,
        request_id: Ulid,
        cluster_id: Option<String>,
        backend_id: Option<String>,
        websocket_context: Option<String>,
        backend: Option<TcpStream>,
        front_buf: Checkout,
        back_buf: Checkout,
        session_address: Option<SocketAddr>,
        protocol: Protocol,
        listener: Rc<RefCell<L>>,
    ) -> Pipe<Front, L> {
        let log_ctx = format!(
            "{} {} {}\t",
            &request_id,
            cluster_id.as_deref().unwrap_or("-"),
            backend_id.as_deref().unwrap_or("-")
        );
        let frontend_status = ConnectionStatus::Normal;
        let backend_status = if backend.is_none() {
            ConnectionStatus::Closed
        } else {
            ConnectionStatus::Normal
        };

        let session = Pipe {
            frontend,
            backend,
            frontend_token,
            backend_token: None,
            frontend_buffer: front_buf,
            backend_buffer: back_buf,
            cluster_id,
            backend_id,
            request_id,
            websocket_context,
            frontend_readiness: Readiness {
                interest: Ready::all(),
                event: Ready::empty(),
            },
            backend_readiness: Readiness {
                interest: Ready::all(),
                event: Ready::empty(),
            },
            log_ctx,
            session_address,
            protocol,
            frontend_status,
            backend_status,
            frontend_timeout: None,
            backend_timeout: None,
            listener,
        };

        trace!("created pipe");
        session
    }

    fn tokens(&self) -> Option<(Token, Token)> {
        if let Some(back) = self.backend_token {
            return Some((self.frontend_token, back));
        }
        None
    }

    pub fn front_socket(&self) -> &TcpStream {
        self.frontend.socket_ref()
    }

    pub fn front_socket_mut(&mut self) -> &mut TcpStream {
        self.frontend.socket_mut()
    }

    pub fn back_socket(&self) -> Option<&TcpStream> {
        self.backend.as_ref()
    }

    pub fn back_socket_mut(&mut self) -> Option<&mut TcpStream> {
        self.backend.as_mut()
    }

    pub fn set_back_socket(&mut self, socket: TcpStream) {
        self.backend = Some(socket);
        self.backend_status = ConnectionStatus::Normal;
    }

    pub fn back_token(&self) -> Vec<Token> {
        self.backend_token.iter().cloned().collect()
    }

    pub fn cancel_timeouts(&mut self) {
        self.frontend_timeout.as_mut().map(|t| t.cancel());
        self.backend_timeout.as_mut().map(|t| t.cancel());
    }

    fn reset_timeouts(&mut self) {
        if let Some(t) = self.frontend_timeout.as_mut() {
            if !t.reset() {
                error!("{} could not reset front timeout (pipe)", self.request_id);
            }
        }

        if let Some(t) = self.backend_timeout.as_mut() {
            if !t.reset() {
                error!("{} could not reset back timeout (pipe)", self.request_id);
            }
        }
    }

    pub fn set_cluster_id(&mut self, cluster_id: Option<String>) {
        self.cluster_id = cluster_id;
        self.reset_log_context();
    }

    pub fn set_backend_id(&mut self, backend_id: Option<String>) {
        self.backend_id = backend_id;
        self.reset_log_context();
    }

    pub fn reset_log_context(&mut self) {
        self.log_ctx = format!(
            "{} {} {}\t",
            self.request_id,
            self.cluster_id.as_deref().unwrap_or("-"),
            self.backend_id.as_deref().unwrap_or("-")
        );
    }

    pub fn set_back_token(&mut self, token: Token) {
        self.backend_token = Some(token);
    }

    pub fn front_readiness(&mut self) -> &mut Readiness {
        &mut self.frontend_readiness
    }

    pub fn back_readiness(&mut self) -> &mut Readiness {
        &mut self.backend_readiness
    }

    pub fn get_session_address(&self) -> Option<SocketAddr> {
        self.session_address
            .or_else(|| self.frontend.socket_ref().peer_addr().ok())
    }

    pub fn get_backend_address(&self) -> Option<SocketAddr> {
        self.backend
            .as_ref()
            .and_then(|backend| backend.peer_addr().ok())
    }

    fn protocol_string(&self) -> &'static str {
        match self.protocol {
            Protocol::TCP => "TCP",
            Protocol::HTTP => "WS",
            Protocol::HTTPS => match self.frontend.protocol() {
                TransportProtocol::Ssl2 => "WSS-SSL2",
                TransportProtocol::Ssl3 => "WSS-SSL3",
                TransportProtocol::Tls1_0 => "WSS-TLS1.0",
                TransportProtocol::Tls1_1 => "WSS-TLS1.1",
                TransportProtocol::Tls1_2 => "WSS-TLS1.2",
                TransportProtocol::Tls1_3 => "WSS-TLS1.3",
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }
    }

    pub fn log_request_success(&self, metrics: &SessionMetrics) {
        let session_addr = match self.get_session_address() {
            None => String::from("-"),
            Some(SocketAddr::V4(addr)) => format!("{}", addr),
            Some(SocketAddr::V6(addr)) => format!("{}", addr),
        };

        let backend = match self.get_backend_address() {
            None => String::from("-"),
            Some(SocketAddr::V4(addr)) => format!("{}", addr),
            Some(SocketAddr::V6(addr)) => format!("{}", addr),
        };

        let response_time = metrics.response_time();
        let service_time = metrics.service_time();

        let cluster_id = self.cluster_id.clone().unwrap_or_else(|| String::from("-"));
        time!(
            "request_time",
            &cluster_id,
            response_time.whole_milliseconds()
        );

        if let Some(backend_id) = metrics.backend_id.as_ref() {
            if let Some(backend_response_time) = metrics.backend_response_time() {
                record_backend_metrics!(
                    cluster_id,
                    backend_id,
                    backend_response_time.whole_milliseconds(),
                    metrics.backend_connection_time(),
                    metrics.backend_bin,
                    metrics.backend_bout
                );
            }
        }

        let proto = self.protocol_string();
        let listener = self.listener.borrow();
        let tags = listener
            .get_tags(&listener.get_addr().to_string())
            .map(|tags| {
                tags.iter()
                    .map(|(k, v)| format!("{}={}", k, v))
                    .collect::<Vec<_>>()
                    .join(", ")
            });

        info_access!(
            "{}{} -> {}\t{} {} {} {}\t{}\t{} {}",
            self.log_ctx,
            session_addr,
            backend,
            LogDuration(response_time),
            LogDuration(service_time),
            metrics.bin,
            metrics.bout,
            OptionalString::from(tags.as_ref()),
            proto,
            self.websocket_context.as_deref().unwrap_or("-")
        );
    }

    pub fn log_request_error(&self, metrics: &SessionMetrics, message: &str) {
        let session = match self.get_session_address() {
            None => String::from("-"),
            Some(SocketAddr::V4(addr)) => format!("{}", addr),
            Some(SocketAddr::V6(addr)) => format!("{}", addr),
        };

        let backend = match self.get_backend_address() {
            None => String::from("-"),
            Some(SocketAddr::V4(addr)) => format!("{}", addr),
            Some(SocketAddr::V6(addr)) => format!("{}", addr),
        };

        let response_time = metrics.response_time();
        let service_time = metrics.service_time();

        let cluster_id = self.cluster_id.clone().unwrap_or_else(|| String::from("-"));
        time!(
            "request_time",
            &cluster_id,
            response_time.whole_milliseconds()
        );

        if let Some(backend_id) = metrics.backend_id.as_ref() {
            if let Some(backend_response_time) = metrics.backend_response_time() {
                record_backend_metrics!(
                    cluster_id,
                    backend_id,
                    backend_response_time.whole_milliseconds(),
                    metrics.backend_connection_time(),
                    metrics.backend_bin,
                    metrics.backend_bout
                );
            }
        }

        let proto = self.protocol_string();

        error_access!(
            "{}{} -> {}\t{} {} {} {}\t{} {} | {}",
            self.log_ctx,
            session,
            backend,
            LogDuration(response_time),
            LogDuration(service_time),
            metrics.bin,
            metrics.bout,
            proto,
            self.websocket_context.as_deref().unwrap_or("-"),
            message
        );
    }

    pub fn check_connections(&self) -> bool {
        match (self.frontend_status, self.backend_status) {
            //(ConnectionStatus::Normal, ConnectionStatus::Normal) => true,
            //(ConnectionStatus::Normal, ConnectionStatus::ReadOpen) => true,
            (ConnectionStatus::Normal, ConnectionStatus::WriteOpen) => {
                // technically we should keep it open, but we'll assume that if the front
                // is not readable and there is no in flight data front -> back or back -> front,
                // we'll close the session, otherwise it interacts badly with HTTP connections
                // with Connection: close header and no Content-length
                self.frontend_readiness.event.is_readable()
                    || self.frontend_buffer.available_data() > 0
                    || self.backend_buffer.available_data() > 0
            }
            (ConnectionStatus::Normal, ConnectionStatus::Closed) => {
                self.backend_buffer.available_data() > 0
            }

            (ConnectionStatus::WriteOpen, ConnectionStatus::Normal) => {
                // technically we should keep it open, but we'll assume that if the back
                // is not readable and there is no in flight data back -> front or front -> back, we'll close the session
                self.backend_readiness.event.is_readable()
                    || self.backend_buffer.available_data() > 0
                    || self.frontend_buffer.available_data() > 0
            }
            //(ConnectionStatus::WriteOpen, ConnectionStatus::ReadOpen) => true,
            (ConnectionStatus::WriteOpen, ConnectionStatus::WriteOpen) => {
                self.frontend_buffer.available_data() > 0
                    || self.backend_buffer.available_data() > 0
            }
            (ConnectionStatus::WriteOpen, ConnectionStatus::Closed) => {
                self.backend_buffer.available_data() > 0
            }

            //(ConnectionStatus::ReadOpen, ConnectionStatus::Normal) => true,
            (ConnectionStatus::ReadOpen, ConnectionStatus::ReadOpen) => false,
            //(ConnectionStatus::ReadOpen, ConnectionStatus::WriteOpen) => true,
            (ConnectionStatus::ReadOpen, ConnectionStatus::Closed) => false,

            (ConnectionStatus::Closed, ConnectionStatus::Normal) => {
                self.frontend_buffer.available_data() > 0
            }
            (ConnectionStatus::Closed, ConnectionStatus::ReadOpen) => false,
            (ConnectionStatus::Closed, ConnectionStatus::WriteOpen) => {
                self.frontend_buffer.available_data() > 0
            }
            (ConnectionStatus::Closed, ConnectionStatus::Closed) => false,

            _ => true,
        }
    }

    pub fn frontend_hup(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        self.log_request_success(metrics);
        self.frontend_status = ConnectionStatus::Closed;
        SessionResult::CloseSession
    }

    pub fn backend_hup(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        self.backend_status = ConnectionStatus::Closed;
        if self.backend_buffer.available_data() == 0 {
            if self.backend_readiness.event.is_readable() {
                self.back_readiness().interest.insert(Ready::readable());
                error!("Pipe::back_hup: backend connection closed but the kernel still holds some data. readiness: {:?} -> {:?}", self.frontend_readiness, self.backend_readiness);
                SessionResult::Continue
            } else {
                self.log_request_success(metrics);
                SessionResult::CloseSession
            }
        } else {
            self.front_readiness().interest.insert(Ready::writable());
            if self.backend_readiness.event.is_readable() {
                self.backend_readiness.interest.insert(Ready::readable());
            }
            SessionResult::Continue
        }
    }

    // Read content from the session
    pub fn readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        self.reset_timeouts();

        trace!("pipe readable");
        if self.frontend_buffer.available_space() == 0 {
            self.frontend_readiness.interest.remove(Ready::readable());
            self.backend_readiness.interest.insert(Ready::writable());
            return SessionResult::Continue;
        }

        let (sz, res) = self.frontend.socket_read(self.frontend_buffer.space());
        debug!(
            "{}\tFRONT [{:?}]: read {} bytes",
            self.log_ctx, self.frontend_token, sz
        );

        if sz > 0 {
            //FIXME: replace with copy()
            self.frontend_buffer.fill(sz);

            count!("bytes_in", sz as i64);
            metrics.bin += sz;

            if self.frontend_buffer.available_space() == 0 {
                self.frontend_readiness.interest.remove(Ready::readable());
            }
            self.backend_readiness.interest.insert(Ready::writable());
        } else {
            self.frontend_readiness.event.remove(Ready::readable());

            if res == SocketResult::Continue {
                self.frontend_status = match self.frontend_status {
                    ConnectionStatus::Normal => ConnectionStatus::WriteOpen,
                    ConnectionStatus::ReadOpen => ConnectionStatus::Closed,
                    s => s,
                };
            }
        }

        if !self.check_connections() {
            metrics.service_stop();
            self.frontend_readiness.reset();
            self.backend_readiness.reset();
            self.log_request_success(metrics);
            return SessionResult::CloseSession;
        }

        match res {
            SocketResult::Error => {
                metrics.service_stop();
                incr!("pipe.errors");
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_error(metrics, "front socket read error");
                return SessionResult::CloseSession;
            }
            SocketResult::Closed => {
                metrics.service_stop();
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_success(metrics);
                return SessionResult::CloseSession;
            }
            SocketResult::WouldBlock => {
                self.frontend_readiness.event.remove(Ready::readable());
            }
            SocketResult::Continue => {}
        };

        self.backend_readiness.interest.insert(Ready::writable());
        SessionResult::Continue
    }

    // Forward content to session
    pub fn writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        trace!("pipe writable");
        if self.backend_buffer.available_data() == 0 {
            self.backend_readiness.interest.insert(Ready::readable());
            self.frontend_readiness.interest.remove(Ready::writable());
            return SessionResult::Continue;
        }

        let mut sz = 0usize;
        let mut res = SocketResult::Continue;
        while res == SocketResult::Continue {
            // no more data in buffer, stop here
            if self.backend_buffer.available_data() == 0 {
                count!("bytes_out", sz as i64);
                metrics.bout += sz;
                self.backend_readiness.interest.insert(Ready::readable());
                self.frontend_readiness.interest.remove(Ready::writable());
                return SessionResult::Continue;
            }
            let (current_sz, current_res) = self.frontend.socket_write(self.backend_buffer.data());
            res = current_res;
            self.backend_buffer.consume(current_sz);
            sz += current_sz;

            if current_sz == 0 && res == SocketResult::Continue {
                self.frontend_status = match self.frontend_status {
                    ConnectionStatus::Normal => ConnectionStatus::ReadOpen,
                    ConnectionStatus::WriteOpen => ConnectionStatus::Closed,
                    s => s,
                };
            }

            if !self.check_connections() {
                metrics.bout += sz;
                count!("bytes_out", sz as i64);
                metrics.service_stop();
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_success(metrics);
                return SessionResult::CloseSession;
            }
        }

        if sz > 0 {
            count!("bytes_out", sz as i64);
            self.backend_readiness.interest.insert(Ready::readable());
            metrics.bout += sz;
        }

        if let Some((front, back)) = self.tokens() {
            debug!(
                "{}\tFRONT [{}<-{}]: wrote {} bytes of {}",
                self.log_ctx,
                front.0,
                back.0,
                sz,
                self.backend_buffer.available_data()
            );
        }

        match res {
            SocketResult::Error => {
                incr!("pipe.errors");
                metrics.service_stop();
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_error(metrics, "front socket write error");
                return SessionResult::CloseSession;
            }
            SocketResult::Closed => {
                metrics.service_stop();
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_success(metrics);
                return SessionResult::CloseSession;
            }
            SocketResult::WouldBlock => {
                self.frontend_readiness.event.remove(Ready::writable());
            }
            SocketResult::Continue => {}
        }

        SessionResult::Continue
    }

    // Forward content to cluster
    pub fn backend_writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        trace!("pipe back_writable");

        if self.frontend_buffer.available_data() == 0 {
            self.frontend_readiness.interest.insert(Ready::readable());
            self.backend_readiness.interest.remove(Ready::writable());
            return SessionResult::Continue;
        }

        let tokens = self.tokens();
        let output_size = self.frontend_buffer.available_data();

        let mut sz = 0usize;
        let mut socket_res = SocketResult::Continue;

        if let Some(ref mut backend) = self.backend {
            while socket_res == SocketResult::Continue {
                // no more data in buffer, stop here
                if self.frontend_buffer.available_data() == 0 {
                    self.frontend_readiness.interest.insert(Ready::readable());
                    self.backend_readiness.interest.remove(Ready::writable());
                    return SessionResult::Continue;
                }

                let (current_sz, current_res) = backend.socket_write(self.frontend_buffer.data());
                socket_res = current_res;
                self.frontend_buffer.consume(current_sz);
                sz += current_sz;

                if current_sz == 0 && current_res == SocketResult::Continue {
                    self.backend_status = match self.backend_status {
                        ConnectionStatus::Normal => ConnectionStatus::ReadOpen,
                        ConnectionStatus::WriteOpen => ConnectionStatus::Closed,
                        s => s,
                    };
                }
            }
        }

        metrics.backend_bout += sz;

        if !self.check_connections() {
            metrics.service_stop();
            self.frontend_readiness.reset();
            self.backend_readiness.reset();
            self.log_request_success(metrics);
            return SessionResult::CloseSession;
        }

        if let Some((front, back)) = tokens {
            debug!(
                "{}\tBACK [{}->{}]: wrote {} bytes of {}",
                self.log_ctx, front.0, back.0, sz, output_size
            );
        }
        match socket_res {
            SocketResult::Error => {
                metrics.service_stop();
                incr!("pipe.errors");
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_error(metrics, "back socket write error");
                return SessionResult::CloseSession;
            }
            SocketResult::Closed => {
                metrics.service_stop();
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                self.log_request_success(metrics);
                return SessionResult::CloseSession;
            }
            SocketResult::WouldBlock => {
                self.backend_readiness.event.remove(Ready::writable());
            }
            SocketResult::Continue => {}
        }
        SessionResult::Continue
    }

    // Read content from cluster
    pub fn backend_readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
        self.reset_timeouts();

        trace!("pipe back_readable");
        if self.backend_buffer.available_space() == 0 {
            self.backend_readiness.interest.remove(Ready::readable());
            return SessionResult::Continue;
        }

        let tokens = self.tokens();

        if let Some(ref mut backend) = self.backend {
            let (size, remaining) = backend.socket_read(self.backend_buffer.space());
            self.backend_buffer.fill(size);

            if let Some((front, back)) = tokens {
                debug!(
                    "{}\tBACK  [{}<-{}]: read {} bytes",
                    self.log_ctx, front.0, back.0, size
                );
            }

            if remaining != SocketResult::Continue || size == 0 {
                self.backend_readiness.event.remove(Ready::readable());
            }
            if size > 0 {
                self.frontend_readiness.interest.insert(Ready::writable());
                metrics.backend_bin += size;
            }

            if size == 0 && remaining == SocketResult::Closed {
                self.backend_status = match self.backend_status {
                    ConnectionStatus::Normal => ConnectionStatus::WriteOpen,
                    ConnectionStatus::ReadOpen => ConnectionStatus::Closed,
                    s => s,
                };

                if !self.check_connections() {
                    metrics.service_stop();
                    self.frontend_readiness.reset();
                    self.backend_readiness.reset();
                    self.log_request_success(metrics);
                    return SessionResult::CloseSession;
                }
            }

            match remaining {
                SocketResult::Error => {
                    metrics.service_stop();
                    incr!("pipe.errors");
                    self.frontend_readiness.reset();
                    self.backend_readiness.reset();
                    self.log_request_error(metrics, "back socket read error");
                    return SessionResult::CloseSession;
                }
                SocketResult::Closed => {
                    if !self.check_connections() {
                        metrics.service_stop();
                        self.frontend_readiness.reset();
                        self.backend_readiness.reset();
                        self.log_request_success(metrics);
                        return SessionResult::CloseSession;
                    }
                }
                SocketResult::WouldBlock => {
                    self.backend_readiness.event.remove(Ready::readable());
                }
                SocketResult::Continue => {}
            }
        }

        SessionResult::Continue
    }

    pub fn log_context(&self) -> LogContext {
        LogContext {
            request_id: self.request_id,
            cluster_id: self.cluster_id.as_deref(),
            backend_id: self.backend_id.as_deref(),
        }
    }
}

impl<Front: SocketHandler, L: ListenerHandler> SessionState for Pipe<Front, L> {
    fn ready(
        &mut self,
        _session: Rc<RefCell<dyn crate::ProxySession>>,
        _proxy: Rc<RefCell<dyn crate::HttpProxyTrait>>,
        metrics: &mut SessionMetrics,
    ) -> super::ProtocolResult {
        let mut counter = 0;
        let max_loop_iterations = 100000;

        if self.frontend_readiness.event.is_hup() {
            return ProtocolResult::Close;
        }

        let token = self.frontend_token;
        while counter < max_loop_iterations {
            let frontend_interest = self.frontend_readiness.filter_interest();
            let backend_interest = self.backend_readiness.filter_interest();

            trace!(
                "PROXY\t{} {:?} {:?} -> {:?}",
                self.log_context(),
                token,
                self.frontend_readiness,
                self.backend_readiness
            );

            if frontend_interest.is_empty() && backend_interest.is_empty() {
                break;
            }

            if self.backend_readiness.event.is_hup()
                && self.frontend_readiness.interest.is_writable()
                && !self.frontend_readiness.event.is_writable()
            {
                break;
            }

            if frontend_interest.is_readable() {
                if self.readable(metrics) == SessionResult::CloseSession {
                    return ProtocolResult::Close;
                }
            }

            if backend_interest.is_writable() {
                if self.backend_writable(metrics) == SessionResult::CloseSession {
                    return ProtocolResult::Close;
                }
            }

            if backend_interest.is_readable() {
                if self.backend_readable(metrics) == SessionResult::CloseSession {
                    return ProtocolResult::Close;
                }
            }

            if frontend_interest.is_writable() {
                if self.writable(metrics) == SessionResult::CloseSession {
                    return ProtocolResult::Close;
                }
            }

            if backend_interest.is_hup() {
                if self.backend_hup(metrics) == SessionResult::CloseSession {
                    return ProtocolResult::Close;
                }
            }

            if frontend_interest.is_error() {
                error!(
                    "PROXY session {:?} front error, disconnecting",
                    self.frontend_token
                );

                self.frontend_readiness.interest = Ready::empty();
                self.backend_readiness.interest = Ready::empty();

                return ProtocolResult::Close;
            }

            if backend_interest.is_error()
                && self.backend_hup(metrics) == SessionResult::CloseSession
            {
                self.frontend_readiness.interest = Ready::empty();
                self.backend_readiness.interest = Ready::empty();

                error!(
                    "PROXY session {:?} back error, disconnecting",
                    self.frontend_token
                );
                return ProtocolResult::Close;
            }

            counter += 1;
        }

        if counter == max_loop_iterations {
            error!(
                "PROXY\thandling session {:?} went through {} iterations, there's a probable infinite loop bug, closing the connection",
                self.frontend_token, max_loop_iterations
            );
            incr!("http.infinite_loop.error");

            let frontend_interest = self.frontend_readiness.filter_interest();
            let backend_interest = self.backend_readiness.filter_interest();

            let token = self.frontend_token;
            let back = self.backend_readiness.clone();
            error!(
                "PROXY\t{:?} readiness: {:?} -> {:?} | front: {:?} | back: {:?} ",
                token, self.frontend_readiness, back, frontend_interest, backend_interest
            );
            self.print_state();

            return ProtocolResult::Close;
        }

        ProtocolResult::Continue
    }

    fn process_events(&mut self, token: Token, events: Ready) {
        if self.frontend_token == token {
            self.frontend_readiness.event |= events;
        } else if self.backend_token == Some(token) {
            self.backend_readiness.event |= events;
        }
    }

    fn timeout(&mut self, token: Token, metrics: &mut SessionMetrics) -> SessionResult {
        //info!("got timeout for token: {:?}", token);
        if self.frontend_token == token {
            self.log_request_error(metrics, "front socket timeout");
            if let Some(timeout) = self.frontend_timeout.as_mut() {
                timeout.triggered();
                SessionResult::CloseSession
            } else {
                SessionResult::CloseSession
            }
        } else if self.backend_token == Some(token) {
            //info!("backend timeout triggered for token {:?}", token);
            if let Some(timeout) = self.backend_timeout.as_mut() {
                timeout.triggered();
            }
            self.log_request_error(metrics, "back socket timeout");
            SessionResult::CloseSession
        } else {
            error!("got timeout for an invalid token");
            self.log_request_error(metrics, "invalid token timeout");
            SessionResult::CloseSession
        }
    }

    fn print_state(&self) {
        todo!()
    }

    fn tokens(&self) -> Vec<Token> {
        todo!()
    }

    fn shutting_down(&mut self) -> super::ProtocolResult {
        todo!()
    }
}

impl<Front: SocketHandler, L: ListenerHandler> std::ops::Drop for Pipe<Front, L> {
    fn drop(&mut self) {
        match self.protocol {
            Protocol::TCP => {}
            _ => gauge_add!("websocket.active_requests", -1),
        }
    }
}
