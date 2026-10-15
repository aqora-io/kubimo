//! A client must honour its config's read timeout. kube's watcher only
//! reconnects after a request errors, so a connection that dies without a
//! FIN or RST (e.g. across a control-plane restart) has to time out rather
//! than leave a controller's watch hanging forever.

use std::net::TcpListener;
use std::time::Duration;

#[tokio::test]
async fn requests_to_a_silent_server_time_out() {
    // kube builds a TLS stack even for plain HTTP, and the workspace compiles
    // in more than one rustls provider, so one has to be picked (as the
    // controller's main does).
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // Accepts connections and never answers on them.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let held: Vec<_> = listener.incoming().collect();
        drop(held);
    });

    let mut config = kubimo::kube::Config::new(format!("http://{addr}").parse().unwrap());
    config.read_timeout = Some(Duration::from_millis(500));
    let client = kubimo::Client::builder()
        .config(config)
        .build()
        .await
        .unwrap();

    let result = tokio::time::timeout(Duration::from_secs(10), client.kube().apiserver_version())
        .await
        .expect("the request hung instead of hitting the read timeout");
    assert!(result.is_err(), "{result:?}");
}
