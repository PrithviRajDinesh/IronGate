use clap::Parser;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{body::Incoming, server::conn::http1, Request, Response};
use hyper_util::{
    client::legacy::Client,
    rt::{TokioExecutor, TokioIo},
    service::TowerToHyperService,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::{rustls, TlsAcceptor};
use tower::Service;
use redis::AsyncCommands;

pub struct Backend {
    pub addr: String,
    pub is_alive: std::sync::atomic::AtomicBool,
}

pub struct BackendRegistry {
    pub backends: Vec<Backend>,
    pub curr: AtomicUsize,
}

impl BackendRegistry {
    pub fn round_robin(&self) -> Option<&Backend> {
        let len = self.backends.len();
        for _ in 0..len {
            let ind = self.curr.fetch_add(1, Ordering::SeqCst) % len;
            let backend = &self.backends[ind];

            if backend.is_alive.load(Ordering::SeqCst) {
                return Some(backend);
            }
        }
        None
    }
}

pub struct LbState {
    pub registry: BackendRegistry,
    pub client: Client<hyper_util::client::legacy::connect::HttpConnector, Incoming>,
    pub redis: redis::aio::ConnectionManager,
}

#[derive(Parser)]
struct Auth {
    #[arg(long, default_value = "127.0.0.1:8080")]
    addr: String,

    #[arg(long)]
    key: PathBuf,

    #[arg(long)]
    cert: PathBuf,
}

#[derive(Clone)]
pub struct Logger<S> {
    inner: S,
}

impl<S> Logger<S> {
    pub fn new(inner: S) -> Self {
        Logger { inner }
    }
}

impl<S> Service<Request<Incoming>> for Logger<S>
where
    S: Service<Request<Incoming>> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Incoming>) -> Self::Future {
        println!("LOG: {} {}", req.method(), req.uri().path());
        self.inner.call(req)
    }
}

async fn handle_stats_request(
    state: Arc<LbState>
) -> Result<Response<Full<Bytes>>, Box<dyn std::error::Error + Send + Sync>> {
    let mut redis_conn = state.redis.clone();
    let keys: Vec<String> = redis_conn.keys("lb:hits:*").await?;

    let mut stats_html = String::from("
        <html>
        <head>
            <title>Iron Gate Live</title>
            <style>
                body { font-family: monospace; background-color: #121212; color: #00ff00; padding: 2rem; }
                li { font-size: 1.2rem; margin-bottom: 0.5rem; }
            </style>
        </head>
        <body>
            <h1>Iron Gate Live Stats</h1>
            <ul id='stats-list'>
    ");

    for key in keys {
        let hits: u64 = redis_conn.get(&key).await.unwrap_or(0);
        let clean_key = key.replace("lb:hits:", "");
        stats_html.push_str(&format!("<li><b>{}</b>: {} hits</li>", clean_key, hits));
    }

    stats_html.push_str("
            </ul>
            <script>
                // Run this every 1000 milliseconds (1 second)
                setInterval(() => {
                    fetch('/stats')
                        .then(response => response.text())
                        .then(html => {
                            // Parse the incoming HTML quietly in the background
                            let parser = new DOMParser();
                            let doc = parser.parseFromString(html, 'text/html');
                            // Replace the old list with the newly fetched list
                            document.getElementById('stats-list').innerHTML = doc.getElementById('stats-list').innerHTML;
                        });
                }, 1000);
            </script>
        </body>
        </html>
    ");

    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html")
        .body(Full::new(Bytes::from(stats_html)))
        .unwrap()
    )
}

async fn lb_handler(
    mut req: Request<Incoming>,
    state: Arc<LbState>,
) -> Result<Response<Full<Bytes>>, Box<dyn std::error::Error + Send + Sync>> {
    let client_ip = req
        .extensions()
        .get::<std::net::SocketAddr>()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let path = req.uri().path();

    if path == "/stats"{
        return handle_stats_request(state).await;
    }

    let target_backend = match state.registry.round_robin() {
        Some(b) => b,
        None => {
            eprintln!("CRITICAL: No Healthy backends found!");
            return Ok(Response::builder()
                .status(503)
                .body("No healthy backends available".into())
                .unwrap());
        }
    };

    let target_backend_addr = target_backend.addr.clone();

    let uri_string = format!("http://{}{}", target_backend.addr, path);
    let uri = uri_string.parse::<hyper::Uri>()?;
    *req.uri_mut() = uri;

    req.headers_mut()
        .insert("X-Forwarded-For", client_ip.parse()?);

    let response = state.client.request(req).await?;

    if response.status().is_success(){
        let mut redis_conn = state.redis.clone();
        let stats_key = format!("lb:hits:{}", target_backend_addr);

        tokio::spawn(async move{
            if let Err(e) = redis_conn.incr::<_, _, ()>(stats_key, 1).await{
                eprintln!("Redis Stats Error: {}", e);
        }
    });
}

    let (parts, body) = response.into_parts();

    let collected_body = body.collect().await?.to_bytes();

    Ok(Response::from_parts(parts, Full::new(collected_body)))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Auth::parse();

    let redis_client = redis::Client::open("redis://127.0.0.1/")?;
    let redis_manager = redis_client
        .get_connection_manager()
        .await?;

    let state = Arc::new(LbState {
        registry: BackendRegistry {
            backends: vec![
                Backend {
                    addr: "127.0.0.1:9001".to_string(),
                    is_alive: std::sync::atomic::AtomicBool::new(true),
                },
                Backend {
                    addr: "127.0.0.1:9002".to_string(),
                    is_alive: std::sync::atomic::AtomicBool::new(true),
                },
            ],
            curr: AtomicUsize::new(0),
        },
        client: Client::builder(TokioExecutor::new()).build_http(),
        redis: redis_manager,
    });

    let certs = CertificateDer::pem_file_iter(&args.cert)?.collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_file(&args.key)?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    let acceptor = TlsAcceptor::from(Arc::new(config));
    let addr = args.addr.to_socket_addrs()?.next().unwrap();
    let listener = TcpListener::bind(&addr).await?;

    println!("LBRust active on https://{}", addr);

    let health_state = Arc::clone(&state);

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            for backend in &health_state.registry.backends {
                let check = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    tokio::net::TcpStream::connect(&backend.addr),
                )
                .await;

                match check {
                    Ok(Ok(_stream)) => {
                        backend.is_alive.store(true, Ordering::SeqCst);
                    }
                    _ => {
                        eprintln!("Health check Failed for {}", backend.addr);
                        backend.is_alive.store(false, Ordering::SeqCst);
                    }
                }
            }
        }
    });

    loop {
        let (raw_socket, peer_addr) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let state_for_task = Arc::clone(&state);

        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(raw_socket).await {
                Ok(stream) => stream,
                Err(e) => {
                    eprintln!("Handshake failed: {}", e);
                    return;
                }
            };

            let io = TokioIo::new(tls_stream);
            let task_state = Arc::clone(&state_for_task);

            let svc = tower::service_fn(move |mut req: Request<Incoming>| {
                req.extensions_mut().insert(peer_addr);

                lb_handler(req, Arc::clone(&task_state))
            });
            let logger_svc = Logger::new(svc);
            let final_svc = TowerToHyperService::new(logger_svc);

            if let Err(err) = http1::Builder::new().serve_connection(io, final_svc).await {
                eprintln!("Connection error: {}", err);
            }
        });
    }
}
