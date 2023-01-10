use std::{cell::RefCell, rc::Rc};

use mio::{net::TcpStream, *};
use nom::{Err, HexDisplay};
use rusty_ulid::Ulid;

use crate::{
    pool::Checkout,
    protocol::{http::LogContext, pipe::Pipe},
    protocol::{ProtocolResult, SessionState},
    socket::{SocketHandler, SocketResult},
    sozu_command::ready::Ready,
    tcp::TcpListener,
    Protocol, Readiness, SessionMetrics, SessionResult,
};

use super::{header::ProxyAddr, parser::parse_v2_header};

#[derive(Clone, Copy)]
pub enum HeaderLen {
    V4,
    V6,
    Unix,
}

pub struct ExpectProxyProtocol<Front: SocketHandler> {
    pub addresses: Option<ProxyAddr>,
    frontend_buffer: [u8; 232],
    pub frontend_readiness: Readiness,
    pub frontend_token: Token,
    pub frontend: Front,
    header_len: HeaderLen,
    index: usize,
    pub request_id: Ulid,
}

impl<Front: SocketHandler> ExpectProxyProtocol<Front> {
    pub fn new(frontend: Front, frontend_token: Token, request_id: Ulid) -> Self {
        ExpectProxyProtocol {
            frontend,
            frontend_token,
            request_id,
            frontend_buffer: [0; 232],
            index: 0,
            header_len: HeaderLen::V4,
            frontend_readiness: Readiness {
                interest: Ready::readable(),
                event: Ready::empty(),
            },
            addresses: None,
        }
    }

    pub fn readable(&mut self, metrics: &mut SessionMetrics) -> ProtocolResult {
        let total_len = match self.header_len {
            HeaderLen::V4 => 28,
            HeaderLen::V6 => 52,
            HeaderLen::Unix => 232,
        };

        let (sz, socket_result) = self
            .frontend
            .socket_read(&mut self.frontend_buffer[self.index..total_len]);
        trace!(
            "FRONT proxy protocol [{:?}]: read {} bytes and res={:?}, index = {}, total_len = {}",
            self.frontend_token,
            sz,
            socket_result,
            self.index,
            total_len
        );

        if sz > 0 {
            self.index += sz;

            count!("bytes_in", sz as i64);
            metrics.bin += sz;

            if self.index == self.frontend_buffer.len() {
                self.frontend_readiness.interest.remove(Ready::readable());
            }
        } else {
            self.frontend_readiness.event.remove(Ready::readable());
        }

        match socket_result {
            SocketResult::Error => {
                error!("[{:?}] (expect proxy) front socket error, closing the connection(read {}, wrote {})", self.frontend_token, metrics.bin, metrics.bout);
                metrics.service_stop();
                incr!("proxy_protocol.errors");
                self.frontend_readiness.reset();
                return ProtocolResult::Close;
            }
            SocketResult::WouldBlock => {
                self.frontend_readiness.event.remove(Ready::readable());
            }
            SocketResult::Closed | SocketResult::Continue => {}
        }

        match parse_v2_header(&self.frontend_buffer[..self.index]) {
            Ok((rest, header)) => {
                trace!(
                    "got expect header: {:?}, rest.len() = {}",
                    header,
                    rest.len()
                );
                self.addresses = Some(header.addr);
                ProtocolResult::Upgrade
            }
            Err(Err::Incomplete(_)) => {
                match self.header_len {
                    HeaderLen::V4 => {
                        if self.index == 28 {
                            self.header_len = HeaderLen::V6;
                        }
                    }
                    HeaderLen::V6 => {
                        if self.index == 52 {
                            self.header_len = HeaderLen::Unix;
                        }
                    }
                    HeaderLen::Unix => {
                        if self.index == 232 {
                            error!(
                                "[{:?}] front socket parse error, closing the connection",
                                self.frontend_token
                            );
                            metrics.service_stop();
                            incr!("proxy_protocol.errors");
                            self.frontend_readiness.reset();
                            return ProtocolResult::Continue;
                        }
                    }
                };
                ProtocolResult::Continue
            }
            Err(Err::Error(e)) | Err(Err::Failure(e)) => {
                error!("[{:?}] expect proxy protocol front socket parse error, closing the connection:\n{}", self.frontend_token, e.input.to_hex(16));
                metrics.service_stop();
                incr!("proxy_protocol.errors");
                self.frontend_readiness.reset();
                ProtocolResult::Close
            }
        }
    }

    pub fn front_socket(&self) -> &TcpStream {
        self.frontend.socket_ref()
    }

    pub fn front_socket_mut(&mut self) -> &mut TcpStream {
        self.frontend.socket_mut()
    }

    pub fn readiness(&mut self) -> &mut Readiness {
        &mut self.frontend_readiness
    }

    pub fn into_pipe(
        self,
        front_buf: Checkout,
        back_buf: Checkout,
        backend_socket: Option<TcpStream>,
        backend_token: Option<Token>,
        listener: Rc<RefCell<TcpListener>>,
    ) -> Pipe<Front, TcpListener> {
        let addr = self.front_socket().peer_addr().ok();

        let mut pipe = Pipe::new(
            self.frontend,
            self.frontend_token,
            self.request_id,
            None,
            None,
            None,
            backend_socket,
            front_buf,
            back_buf,
            addr,
            Protocol::TCP,
            listener,
        );

        pipe.frontend_readiness.event = self.frontend_readiness.event;

        if let Some(backend_token) = backend_token {
            pipe.set_back_token(backend_token);
        }

        pipe
    }

    pub fn log_context(&self) -> LogContext {
        LogContext {
            request_id: self.request_id,
            cluster_id: None,
            backend_id: None,
        }
    }
}

impl<Front: SocketHandler> SessionState for ExpectProxyProtocol<Front> {
    fn ready(
        &mut self,
        _session: Rc<RefCell<dyn crate::ProxySession>>,
        _proxy: Rc<RefCell<dyn crate::HttpProxyTrait>>,
        metrics: &mut SessionMetrics,
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
                let protocol_result = self.readable(metrics);
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

    fn update_readiness(&mut self, token: Token, events: Ready) {
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
}

#[cfg(test)]
mod expect_test {

    use super::*;
    use mio::net::TcpListener;
    use rusty_ulid::Ulid;
    use std::io::Write;
    use std::net::TcpStream as StdTcpStream;
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{Arc, Barrier},
        thread::{self, JoinHandle},
    };

    use crate::protocol::proxy_protocol::header::*;

    // Flow diagram of the test below
    //                [connect]   [send proxy protocol]
    //upfront proxy  ----------------------X
    //              /     |           |
    //  sozu     ---------v-----------v----X
    #[test]
    fn middleware_should_receive_proxy_protocol_header_from_an_upfront_middleware() {
        setup_test_logger!();
        let middleware_addr: SocketAddr = "127.0.0.1:3500".parse().expect("parse address error");
        let barrier = Arc::new(Barrier::new(2));

        let upfront = start_upfront_middleware(middleware_addr, barrier.clone());
        start_middleware(middleware_addr, barrier);

        upfront.join().expect("should join");
    }

    // Accept connection from an upfront proxy and expect to read a proxy protocol header in this stream.
    fn start_middleware(middleware_addr: SocketAddr, barrier: Arc<Barrier>) {
        let upfront_middleware_conn_listener = TcpListener::bind(middleware_addr)
            .expect("could not accept upfront middleware connection");
        let session_stream;
        barrier.wait();

        // mio::TcpListener use a nonblocking mode so we have to loop on accept
        loop {
            if let Ok((stream, _addr)) = upfront_middleware_conn_listener.accept() {
                session_stream = stream;
                break;
            }
        }

        let mut session_metrics = SessionMetrics::new(None);
        let mut expect_pp = ExpectProxyProtocol::new(session_stream, Token(0), Ulid::generate());

        let mut res = ProtocolResult::Continue;
        while res == ProtocolResult::Continue {
            res = expect_pp.readable(&mut session_metrics);
        }

        if res != ProtocolResult::Upgrade {
            panic!(
                "Should receive a complete proxy protocol header, res = {:?}",
                res
            );
        };
    }

    // Connect to the next middleware and send a proxy protocol header
    fn start_upfront_middleware(
        next_middleware_addr: SocketAddr,
        barrier: Arc<Barrier>,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            let src_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(125, 25, 10, 1)), 8080);
            let dst_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 4, 5, 8)), 4200);
            let proxy_protocol = HeaderV2::new(Command::Local, src_addr, dst_addr).into_bytes();

            barrier.wait();
            match StdTcpStream::connect(&next_middleware_addr) {
                Ok(mut stream) => {
                    stream.write(&proxy_protocol).unwrap();
                }
                Err(e) => panic!("could not connect to the next middleware: {}", e),
            };
        })
    }
}
