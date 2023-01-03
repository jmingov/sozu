use std::{io::ErrorKind, rc::Rc, cell::RefCell};

use mio::{net::*, Token};
use rustls::ServerConnection;
use rusty_ulid::Ulid;

use crate::{protocol::ProtocolResult, Readiness, Ready, SessionResult, SessionMetrics};

use super::{SessionState, http::LogContext};

pub enum TlsState {
    Initial,
    Handshake,
    Established,
    Error,
}

pub struct TlsHandshake {
    pub stream: TcpStream,
    pub session: ServerConnection,
    pub frontend_readiness: Readiness,
    pub frontend_token: Token,
    pub request_id: Ulid,
}

impl TlsHandshake {
    pub fn new(session: ServerConnection, stream: TcpStream, frontend_token: Token, request_id: Ulid) -> TlsHandshake {
        TlsHandshake {
            stream,
            session,
            frontend_readiness: Readiness {
                interest: Ready::readable() | Ready::hup() | Ready::error(),
                event: Ready::empty(),
            },
            frontend_token,
            request_id,
        }
    }

    pub fn readable(&mut self) -> ProtocolResult {
        let mut can_read = true;

        loop {
            let mut can_work = false;

            if self.session.wants_read() && can_read {
                can_work = true;

                match self.session.read_tls(&mut self.stream) {
                    Ok(0) => {
                        error!("connection closed during handshake");
                        return ProtocolResult::Close;
                    }
                    Ok(_) => {}
                    Err(e) => match e.kind() {
                        ErrorKind::WouldBlock => {
                            self.frontend_readiness.event.remove(Ready::readable());
                            can_read = false
                        }
                        _ => {
                            error!("could not perform handshake: {:?}", e);
                            return ProtocolResult::Close;
                        }
                    },
                }

                if let Err(e) = self.session.process_new_packets() {
                    error!("could not perform handshake: {:?}", e);
                    return ProtocolResult::Close;
                }
            }

            if !can_work {
                break;
            }
        }

        if !self.session.wants_read() {
            self.frontend_readiness.interest.remove(Ready::readable());
        }

        if self.session.wants_write() {
            self.frontend_readiness.interest.insert(Ready::writable());
        }

        if self.session.is_handshaking() {
            ProtocolResult::Continue
        } else {
            // handshake might be finished but we still have something to send
            if self.session.wants_write() {
                ProtocolResult::Continue
            } else {
                self.frontend_readiness.interest.insert(Ready::readable());
                self.frontend_readiness.event.insert(Ready::readable());
                self.frontend_readiness.interest.insert(Ready::writable());
                ProtocolResult::Upgrade
            }
        }
    }

    pub fn writable(&mut self) -> ProtocolResult {
        let mut can_write = true;

        loop {
            let mut can_work = false;

            if self.session.wants_write() && can_write {
                can_work = true;

                match self.session.write_tls(&mut self.stream) {
                    Ok(_) => {}
                    Err(e) => match e.kind() {
                        ErrorKind::WouldBlock => {
                            self.frontend_readiness.event.remove(Ready::writable());
                            can_write = false
                        }
                        _ => {
                            error!("could not perform handshake: {:?}", e);
                            return ProtocolResult::Close;
                        }
                    },
                }

                if let Err(e) = self.session.process_new_packets() {
                    error!("could not perform handshake: {:?}", e);
                    return ProtocolResult::Close;
                }
            }

            if !can_work {
                break;
            }
        }

        if !self.session.wants_write() {
            self.frontend_readiness.interest.remove(Ready::writable());
        }

        if self.session.wants_read() {
            self.frontend_readiness.interest.insert(Ready::readable());
        }

        if self.session.is_handshaking() {
            ProtocolResult::Continue
        } else if self.session.wants_read() {
            self.frontend_readiness.interest.insert(Ready::readable());
            ProtocolResult::Upgrade
        } else {
            self.frontend_readiness.interest.insert(Ready::writable());
            self.frontend_readiness.interest.insert(Ready::readable());
            ProtocolResult::Upgrade
        }
    }

    pub fn log_context(&self) -> LogContext {
        LogContext {
            request_id: self.request_id,
            cluster_id: None,
            backend_id: None,
        }
    }
}

impl SessionState for TlsHandshake {
    fn ready(
        &mut self,
        _session: Rc<RefCell<dyn crate::ProxySession>>,
        _proxy: Rc<RefCell<dyn crate::HttpProxyTrait>>,
        _metrics: &mut SessionMetrics,
    ) -> ProtocolResult {
        let mut counter = 0;
        let max_loop_iterations = 100000;

        if self.frontend_readiness.event.is_hup() {
            return ProtocolResult::Close;
        }

        while counter < max_loop_iterations {
            let frontend_interest = self.frontend_readiness.filter_interest();

            trace!(
                "PROXY\t{} {:?} {:?} -> None",
                self.log_context(),
                self.frontend_token,
                self.frontend_readiness
            );

            if frontend_interest.is_empty() {
                break;
            }

            if frontend_interest.is_readable() {
                let protocol_result = self.readable();
                if protocol_result != ProtocolResult::Continue {
                    return protocol_result;
                }
            }

            if frontend_interest.is_writable() {
                let protocol_result = self.writable();
                if protocol_result != ProtocolResult::Continue {
                    return protocol_result;
                }
            }

            if frontend_interest.is_error() {
                error!(
                    "PROXY session {:?} front error, disconnecting",
                    self.frontend_token
                );
                self.frontend_readiness.interest = Ready::empty();

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

            error!(
                "PROXY\t{:?} readiness: {:?} -> None | front: {:?}",
                self.frontend_token, self.frontend_readiness, frontend_interest
            );
            self.print_state();

            return ProtocolResult::Close;
        }

        ProtocolResult::Continue
    }

    fn process_events(&mut self, token: Token, events: Ready) {
        if self.frontend_token == token {
            self.frontend_readiness.event |= events;
        }
    }

    fn timeout(&mut self, _token: Token, _metrics: &mut SessionMetrics) -> SessionResult {
        // relevant timeout is still stored in the Session as front_timeout.
        todo!();
    }

    fn print_state(&self) {
        todo!()
    }

    fn tokens(&self) -> Vec<Token> {
        todo!()
    }

    fn shutting_down(&mut self) -> ProtocolResult {
        todo!()
    }
}
