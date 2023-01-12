use std::{
    thread,
    time::{Duration, Instant},
};

use serial_test::serial;

use sozu_command_lib::{
    config::FileConfig,
    proxy::{
        ActivateListener, AddCertificate, CertificateAndKey, HttpFrontend, ListenerType,
        ProxyRequestOrder,
    },
    state::ConfigState,
};

use crate::{
    http_utils::http_request,
    mock::{
        aggregator::SimpleAggregator,
        async_backend::BackendHandle as AsyncBackend,
        client::Client,
        https_client::{build_https_client, resolve_request},
        sync_backend::Backend as SyncBackend,
    },
    sozu::worker::Worker,
    tests::{async_setup_test, repeat_until_error_or, sync_setup_test, State},
};

pub fn try_async(nb_backends: usize, nb_clients: usize, nb_requests: usize) -> State {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) =
        async_setup_test(config, listeners, state, front_address, nb_backends);

    let mut clients = (0..nb_clients)
        .map(|i| {
            Client::new(
                format!("client{}", i),
                front_address,
                http_request("GET", "/api", format!("ping{}", i)),
            )
        })
        .collect::<Vec<_>>();
    for client in clients.iter_mut() {
        client.connect();
    }
    for _ in 0..nb_requests {
        for client in clients.iter_mut() {
            client.send();
        }
        for client in clients.iter_mut() {
            match client.receive() {
                Some(response) => println!("{}", response),
                _ => {}
            }
        }
    }

    worker.send_proxy_request(ProxyRequestOrder::SoftStop);
    worker.wait_for_server_stop();

    for client in &clients {
        println!(
            "{} sent: {}, received: {}",
            client.name, client.requests_sent, client.responses_received
        );
    }
    for backend in backends.iter_mut() {
        let aggregator = backend.stop_and_get_aggregator();
        println!("{} aggregated: {:?}", backend.name, aggregator);
    }

    if clients.iter().all(|client| {
        client.requests_sent == nb_requests && client.responses_received == nb_requests
    }) {
        State::Success
    } else {
        State::Fail
    }
}

pub fn try_sync(nb_clients: usize, nb_requests: usize) -> State {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = sync_setup_test(config, listeners, state, front_address, 1);
    let mut backend = backends.pop().unwrap();

    backend.connect();

    let mut clients = (0..nb_clients)
        .map(|i| {
            Client::new(
                format!("client{}", i),
                front_address,
                http_request("GET", "/api", format!("ping{}", i)),
            )
        })
        .collect::<Vec<_>>();

    // send one request, then maintain a keepalive session
    for (i, client) in clients.iter_mut().enumerate() {
        client.connect();
        client.send();
        backend.accept(i);
        backend.receive(i);
        backend.send(i);
        client.receive();
    }

    for _ in 0..nb_requests - 1 {
        for client in clients.iter_mut() {
            client.send();
        }
        for i in 0..nb_clients {
            backend.receive(i);
            backend.send(i);
        }
        for client in clients.iter_mut() {
            match client.receive() {
                Some(response) => println!("{}", response),
                _ => {}
            }
        }
    }

    worker.send_proxy_request(ProxyRequestOrder::SoftStop);
    worker.wait_for_server_stop();

    for client in &clients {
        println!(
            "{} sent: {}, received: {}",
            client.name, client.requests_sent, client.responses_received
        );
    }
    println!(
        "{} sent: {}, received: {}",
        backend.name, backend.responses_sent, backend.requests_received
    );

    if clients.iter().all(|client| {
        client.requests_sent == nb_requests && client.responses_received == nb_requests
    }) {
        State::Success
    } else {
        State::Fail
    }
}

pub fn try_backend_stop(nb_requests: usize, zombie: Option<u32>) -> State {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let config = Worker::into_config(FileConfig {
        zombie_check_interval: zombie,
        ..Worker::empty_file_config()
    });
    let listeners = Worker::empty_listeners();
    let state = ConfigState::new();
    let (mut worker, mut backends) = async_setup_test(config, listeners, state, front_address, 2);
    let mut backend2 = backends.pop().expect("backend2");
    let mut backend1 = backends.pop().expect("backend1");

    let mut aggregator = Some(SimpleAggregator {
        requests_received: 0,
        responses_sent: 0,
    });

    let mut client = Client::new("client", front_address, http_request("GET", "/api", "ping"));
    client.connect();

    let start = Instant::now();
    for i in 0..nb_requests {
        if client.send().is_none() {
            break;
        }
        match client.receive() {
            Some(response) => println!("{}", response),
            None => break,
        }
        if i == 0 {
            aggregator = backend1.stop_and_get_aggregator();
        }
    }
    let duration = Instant::now().duration_since(start);

    worker.send_proxy_request(ProxyRequestOrder::SoftStop);
    let success = worker.wait_for_server_stop();

    println!(
        "sent: {}, received: {}",
        client.requests_sent, client.responses_received
    );
    println!("backend1 aggregator: {:?}", aggregator);
    aggregator = backend2.stop_and_get_aggregator();
    println!("backend2 aggregator: {:?}", aggregator);

    if !success {
        State::Fail
    } else if duration > Duration::from_millis(100) {
        // Reconnecting to unother backend should have lasted less that 100 miliseconds
        State::Undecided
    } else {
        State::Success
    }
}

pub fn try_issue_810_timeout() -> State {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = sync_setup_test(config, listeners, state, front_address, 1);
    let mut backend = backends.pop().unwrap();

    let mut client = Client::new("client", front_address, http_request("GET", "/api", "ping"));

    backend.connect();
    client.connect();
    client.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    client.receive();

    worker.send_proxy_request(ProxyRequestOrder::SoftStop);
    let start = Instant::now();
    let success = worker.wait_for_server_stop();
    let duration = Instant::now().duration_since(start);

    println!(
        "{} sent: {}, received: {}",
        client.name, client.requests_sent, client.responses_received
    );
    println!(
        "{} sent: {}, received: {}",
        backend.name, backend.responses_sent, backend.requests_received
    );

    if !success || duration > Duration::from_millis(100) {
        State::Fail
    } else {
        State::Success
    }
}

pub fn try_issue_810_panic(part2: bool) -> State {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");
    let back_address = format!("127.0.0.1:2002")
        .parse()
        .expect("could not parse back address");

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("WORKER", config, &listeners, state);

    worker.send_proxy_request(ProxyRequestOrder::AddTcpListener(
        Worker::default_tcp_listener(front_address),
    ));
    worker.send_proxy_request(ProxyRequestOrder::ActivateListener(ActivateListener {
        address: front_address,
        proxy: ListenerType::TCP,
        from_scm: false,
    }));
    worker.send_proxy_request(ProxyRequestOrder::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));
    worker.send_proxy_request(ProxyRequestOrder::AddTcpFrontend(
        Worker::default_tcp_frontend("cluster_0", front_address),
    ));

    worker.send_proxy_request(ProxyRequestOrder::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
    )));
    worker.read_to_last();

    let mut backend = SyncBackend::new("backend", back_address, "pong");
    let mut client = Client::new("client", front_address, "ping");

    backend.connect();
    client.connect();
    client.send();
    if !part2 {
        backend.accept(0);
        backend.receive(0);
        backend.send(0);
        let response = client.receive();
        println!("Response: {:?}", response);
    }

    worker.send_proxy_request(ProxyRequestOrder::SoftStop);
    let success = worker.wait_for_server_stop();

    println!(
        "{} sent: {}, received: {}",
        client.name, client.requests_sent, client.responses_received
    );
    println!(
        "{} sent: {}, received: {}",
        backend.name, backend.responses_sent, backend.requests_received
    );

    if success {
        State::Success
    } else {
        State::Fail
    }
}

pub fn try_tls_endpoint() -> State {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");
    let back_address = format!("127.0.0.1:2002")
        .parse()
        .expect("could not parse back address");

    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("WORKER", config, &listeners, state);

    worker.send_proxy_request(ProxyRequestOrder::AddHttpsListener(
        Worker::default_https_listener(front_address),
    ));
    worker.send_proxy_request(ProxyRequestOrder::ActivateListener(ActivateListener {
        address: front_address,
        proxy: ListenerType::HTTPS,
        from_scm: false,
    }));

    worker.send_proxy_request(ProxyRequestOrder::AddCluster(Worker::default_cluster(
        "cluster_0",
    )));

    worker.send_proxy_request(ProxyRequestOrder::AddHttpsFrontend(HttpFrontend {
        hostname: String::from("lolcatho.st"),
        ..Worker::default_http_frontend("cluster_0", front_address)
    }));

    let certificate_and_key = CertificateAndKey {
        certificate: String::from(include_str!("../../../lib/assets/certificate.pem")),
        key: String::from(include_str!("../../../lib/assets/key.pem")),
        certificate_chain: vec![],
        versions: vec![],
    };
    let add_certificate = AddCertificate {
        address: front_address,
        certificate: certificate_and_key,
        names: vec![],
        expired_at: None,
    };
    worker.send_proxy_request(ProxyRequestOrder::AddCertificate(add_certificate));

    worker.send_proxy_request(ProxyRequestOrder::AddBackend(Worker::default_backend(
        "cluster_0",
        "cluster_0-0",
        back_address,
    )));
    worker.read_to_last();

    let mut backend = AsyncBackend::spawn_detached_backend(
        "BACKEND",
        back_address,
        SimpleAggregator::default(),
        AsyncBackend::http_handler("pong"),
    );

    let client = build_https_client();
    let request = client.get("https://lolcatho.st:2001/api".parse().unwrap());
    if let Some((status, body)) = resolve_request(request) {
        println!("response status: {:?}", status);
        println!("response body: {}", body);
    } else {
        return State::Fail;
    }

    worker.send_proxy_request(ProxyRequestOrder::SoftStop);
    let success = worker.wait_for_server_stop();

    let aggregator = backend
        .stop_and_get_aggregator()
        .expect("Could not get aggregator");
    println!(
        "{} sent: {}, received: {}",
        backend.name, aggregator.responses_sent, aggregator.requests_received
    );

    if success && aggregator.responses_sent == 1 {
        State::Success
    } else {
        State::Fail
    }
}

pub fn test_upgrade() -> State {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = sync_setup_test(config, listeners, state, front_address, 1);

    let mut backend = backends.pop().expect("backend");
    let mut client = Client::new("client", front_address, http_request("GET", "/api", "ping"));

    backend.connect();
    client.connect();
    client.send();
    backend.accept(0);
    backend.receive(0);
    backend.send(0);
    match client.receive() {
        Some(msg) => println!("response: {}", msg),
        None => return State::Fail,
    }

    client.send();
    backend.receive(0);
    let mut new_worker = worker.upgrade("NEW_WORKER");
    thread::sleep(Duration::from_millis(100));
    backend.send(0);
    match client.receive() {
        Some(msg) => println!("response: {}", msg),
        None => return State::Fail,
    }
    client.connect();
    client.send();
    println!("ACCEPTING...");
    backend.accept(1);
    backend.receive(1);
    backend.send(1);
    match client.receive() {
        Some(msg) => println!("response: {}", msg),
        None => return State::Fail,
    }

    new_worker.send_proxy_request(ProxyRequestOrder::SoftStop);
    if !worker.wait_for_server_stop() {
        return State::Fail;
    }
    if !new_worker.wait_for_server_stop() {
        return State::Fail;
    }

    println!(
        "{} sent: {}, received: {}",
        client.name, client.requests_sent, client.responses_received
    );
    println!(
        "{} sent: {}, received: {}",
        backend.name, backend.responses_sent, backend.requests_received
    );

    State::Success
}

pub fn test_http(nb_requests: usize) {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = async_setup_test(config, listeners, state, front_address, 1);
    let mut backend = backends.pop().expect("backend");

    let mut bad_client = Client::new(
        format!("bad_client"),
        front_address,
        "GET /api HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\nContent-Length: 3\r\n\r\nbad_ping",
    );
    let mut good_client = Client::new(
        format!("good_client"),
        front_address,
        http_request("GET", "/api", "good_ping"),
    );
    bad_client.connect();
    good_client.connect();

    for _ in 0..nb_requests {
        bad_client.send();
        good_client.send();
        match bad_client.receive() {
            Some(msg) => println!("response: {}", msg),
            None => {}
        }
        match good_client.receive() {
            Some(msg) => println!("response: {}", msg),
            None => {}
        }
    }

    worker.send_proxy_request(ProxyRequestOrder::SoftStop);
    worker.wait_for_server_stop();

    println!(
        "{} sent: {}, received: {}",
        bad_client.name, bad_client.requests_sent, bad_client.responses_received
    );
    println!(
        "{} sent: {}, received: {}",
        good_client.name, good_client.requests_sent, good_client.responses_received
    );
    let aggregator = backend.stop_and_get_aggregator();
    println!("backend aggregator: {:?}", aggregator);
}

pub fn try_hard_or_soft_stop(soft: bool) -> State {
    let front_address = "127.0.0.1:2001"
        .parse()
        .expect("could not parse front address");

    let (config, listeners, state) = Worker::empty_config();
    let (mut worker, mut backends) = sync_setup_test(config, listeners, state, front_address, 1);
    let mut backend = backends.pop().unwrap();

    let mut client = Client::new("client", front_address, http_request("GET", "/api", "ping"));

    // Send a request to try out
    backend.connect();
    client.connect();
    client.send();
    backend.accept(0);
    backend.receive(0);

    // stop sōzu
    if soft {
        // the worker will wait for backends to respond before shutting down
        worker.send_proxy_request(ProxyRequestOrder::SoftStop);
    } else {
        // the worker will shut down without waiting for backends to finish
        worker.send_proxy_request(ProxyRequestOrder::HardStop);
    }
    thread::sleep(Duration::from_millis(100));

    backend.send(0);

    match (soft, client.receive()) {
        (true, None) => {
            println!("SoftStop didn't wait for HTTP response to complete");
            return State::Fail;
        }
        (true, Some(msg)) => {
            println!("response on SoftStop: {}", msg);
        }
        (false, None) => {
            println!("no response on HardStop");
        }
        (false, Some(msg)) => {
            println!("HardStop waited for HTTP response to complete: {}", msg);
            return State::Fail;
        }
    }

    let success = worker.wait_for_server_stop();

    println!(
        "{} sent: {}, received: {}",
        client.name, client.requests_sent, client.responses_received
    );
    println!(
        "{} sent: {}, received: {}",
        backend.name, backend.responses_sent, backend.requests_received
    );

    if success {
        State::Success
    } else {
        State::Fail
    }
}

/*
pub fn test_hard_vs_soft_stop() -> State {
    let state = _test_hard_vs_soft_stop(true);
    if state != State::Success {
        return state;
    }
    _test_hard_vs_soft_stop(false)
}
*/

#[serial]
#[test]
fn test_sync() {
    assert_eq!(try_sync(10, 100), State::Success);
}

#[serial]
#[test]
fn test_async() {
    assert_eq!(try_async(3, 10, 100), State::Success);
}

#[serial]
#[test]
fn test_hard_stop() {
    assert_eq!(
        repeat_until_error_or(
            10,
            "Hard Stop: Test that the worker shuts down even if backends are not done",
            || try_hard_or_soft_stop(false)
        ),
        State::Success
    );
}

#[serial]
#[test]
fn test_soft_stop() {
    assert_eq!(
            repeat_until_error_or(
                10,
                "Hard Stop: Test that the worker waits for all backends to process requests before shutting down",
                || try_hard_or_soft_stop(true)
            ),
            State::Success
        );
}

// https://github.com/sozu-proxy/sozu/issues/806
// This should actually be a success
#[serial]
#[test]
fn test_issue_806() {
    assert!(
        repeat_until_error_or(
            1000,
            "issue 806: timeout with invalid back token\n(not fixed)",
            || try_backend_stop(2, None)
        ) != State::Fail
    );
}

// https://github.com/sozu-proxy/sozu/issues/808
#[serial]
#[test]
fn test_issue_808() {
    assert_eq!(
        repeat_until_error_or(
            1000,
            "issue 808: panic on successful zombie check\n(fixed)",
            || try_backend_stop(2, Some(1))
        ),
        // if Success, it means the session was never a zombie
        // if Fail, it means the zombie checker probably crashed
        State::Undecided
    );
}

// https://github.com/sozu-proxy/sozu/issues/810
#[serial]
#[test]
fn test_issue_810_timeout() {
    assert_eq!(
        repeat_until_error_or(
            1000,
            "issue 810: shutdown struggles until session timeout\n(fixed)",
            try_issue_810_timeout
        ),
        State::Success
    );
}

#[serial]
#[test]
fn test_issue_810_panic_on_session_close() {
    assert_eq!(
        repeat_until_error_or(
            1000,
            "issue 810: shutdown panics on session close\n(fixed)",
            || try_issue_810_panic(false)
        ),
        State::Success
    );
}

#[serial]
#[test]
fn test_issue_810_panic_on_missing_listener() {
    assert_eq!(
            repeat_until_error_or(
                1000,
                "issue 810: shutdown panics on tcp connection after proxy cleared its listeners\n(opinionated fix)",
                || try_issue_810_panic(true)
            ),
            State::Success
        );
}

#[serial]
#[test]
fn test_tls_endpoint() {
    assert_eq!(
        repeat_until_error_or(
            100,
            "TLS endpoint: Sōzu should decrypt an HTTPS request",
            || try_tls_endpoint()
        ),
        State::Success
    );
}
