mod tests;

use std::{io::stdin, net::SocketAddr};

use sozu_command_lib::{
    config::Config,
    proxy::{ActivateListener, ListenerType, ProxyRequestOrder},
    scm_socket::Listeners,
    state::ConfigState,
};

use crate::{
    http_utils::http_ok_response,
    mock::{
        aggregator::SimpleAggregator, async_backend::BackendHandle as AsyncBackend,
        sync_backend::Backend as SyncBackend,
    },
    sozu::worker::Worker,
};

#[derive(PartialEq, Eq, Debug)]
pub enum State {
    Success,
    Fail,
    Undecided,
}

/// Setup a Sozu worker with
/// - `config`
/// - `listeners`
/// - 1 active HttpListener on `front_address`
/// - 1 cluster ("cluster_0")
/// - 1 HttpFrontend for "cluster_0" on `front_address`
/// - n backends ("cluster_0-{0..n}")
pub fn setup_test(
    config: Config,
    listeners: Listeners,
    state: ConfigState,
    front_address: SocketAddr,
    nb_backends: usize,
) -> (Worker, Vec<SocketAddr>) {
    let mut worker = Worker::start_new_worker("WORKER", config, &listeners, state);

    worker.send_proxy_request(ProxyRequestOrder::AddHttpListener(
        Worker::default_http_listener(front_address),
    ));
    worker.send_proxy_request(ProxyRequestOrder::ActivateListener(ActivateListener {
        address: front_address,
        proxy: ListenerType::HTTP,
        from_scm: false,
    }));
    worker.send_proxy_request(ProxyRequestOrder::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request(ProxyRequestOrder::AddHttpFrontend(
        Worker::default_http_frontend("cluster_0", front_address),
    ));

    let mut backends = Vec::new();
    for i in 0..nb_backends {
        let back_address = format!("127.0.0.1:{}", 2002 + i)
            .parse()
            .expect("could not parse back address");
        worker.send_proxy_request(ProxyRequestOrder::AddBackend(Worker::default_backend(
            "cluster_0",
            format!("cluster_0-{}", i),
            back_address,
        )));
        backends.push(back_address);
    }

    worker.read_to_last();
    (worker, backends)
}

pub fn async_setup_test(
    config: Config,
    listeners: Listeners,
    state: ConfigState,
    front_address: SocketAddr,
    nb_backends: usize,
) -> (Worker, Vec<AsyncBackend<SimpleAggregator>>) {
    let (worker, backends) = setup_test(config, listeners, state, front_address, nb_backends);
    let backends = backends
        .into_iter()
        .enumerate()
        .map(|(i, back_address)| {
            let aggregator = SimpleAggregator {
                requests_received: 0,
                responses_sent: 0,
            };
            AsyncBackend::spawn_detached_backend(
                format!("BACKEND_{}", i),
                back_address,
                aggregator.to_owned(),
                AsyncBackend::http_handler(format!("pong{}", i)),
            )
        })
        .collect::<Vec<_>>();
    (worker, backends)
}

pub fn sync_setup_test(
    config: Config,
    listeners: Listeners,
    state: ConfigState,
    front_address: SocketAddr,
    nb_backends: usize,
) -> (Worker, Vec<SyncBackend>) {
    let (worker, backends) = setup_test(config, listeners, state, front_address, nb_backends);
    let backends = backends
        .into_iter()
        .enumerate()
        .map(|(i, back_address)| {
            SyncBackend::new(
                format!("BACKEND_{}", i),
                back_address,
                http_ok_response(format!("pong{}", i)),
            )
        })
        .collect::<Vec<_>>();
    (worker, backends)
}

pub fn repeat_until_error_or<F>(times: usize, test_description: &str, test: F) -> State
where
    F: Fn() -> State + Sized,
{
    println!("{}", test_description);
    for i in 1..=times {
        let state = test();
        match state {
            State::Success => {}
            State::Fail => {
                println!("------------------------------------------------------------------");
                println!("Test not successful after: {} iterations", i);
                return State::Fail;
            }
            State::Undecided => {
                println!("------------------------------------------------------------------");
                println!("Test interupted after: {} iterations", i);
                return State::Undecided;
            }
        }
    }
    println!("------------------------------------------------------------------");
    println!("Test successful after: {} iterations", times);
    State::Success
}

pub fn wait_input<S: Into<String>>(s: S) {
    println!("==================================================================");
    println!("{}", s.into());
    println!("==================================================================");
    let mut buf = String::new();
    stdin().read_line(&mut buf).expect("bad input");
}
