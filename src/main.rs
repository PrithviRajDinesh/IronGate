use clap::Parser;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{body::Incoming, server::conn::http1, Request, Response};
use hyper_util::{
    client::legacy::Client,
    rt::{TokioExecutor, TokioIo},
    service::TowerToHyperService,
};
use http_body_util::combinators::BoxBody;
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
use redis::Script;
use std::hash::{Hash, Hasher};
use std::collections::hash_map::DefaultHasher;
mod config; 
use config::ConfigManager; // Brings your manager into scope
use moka::future::Cache;
use std::time::Duration;

const RATE_LIMIT_SCRIPT: &str = r#"
    local key = KEYS[1]
    local max_requests = tonumber(ARGV[1])
    local window_seconds = tonumber(ARGV[2])

    local count = redis.call('INCR', key)

    if count == 1 then
      redis.call('EXPIRE', key, window_seconds)
    end

    local pttl = redis.call('PTTL', key)

    return { count, pttl }
"#;

pub struct Backend {
    pub addr: String,
    pub is_alive: std::sync::atomic::AtomicBool,
    pub active_connections: std::sync::atomic::AtomicUsize,
    pub consecutive_errors: AtomicUsize,
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

    pub fn ip_hash(&self, client_ip: &str) ->Option<&Backend> {
        let healthy_backends: Vec<&Backend> = self
            .backends
            .iter()
            .filter(|b| b.is_alive.load(Ordering::SeqCst))
            .collect();
        if healthy_backends.is_empty() {
            return None;
        }

        let mut hasher = DefaultHasher::new();
        client_ip.hash(&mut hasher);
        let hash_value = hasher.finish();

        let index = (hash_value as usize) % healthy_backends.len();
        Some(healthy_backends[index])
    }

    pub fn least_connections(&self) -> Option<&Backend> {
        self.backends
            .iter()
            .filter(|b| b.is_alive.load(Ordering::SeqCst))
            .min_by_key(|b| b.active_connections.load(Ordering::SeqCst))
    }
}

pub struct LbState {
    pub registry: BackendRegistry,
    pub config: Arc<ConfigManager>,
    pub client: Client<hyper_util::client::legacy::connect::HttpConnector, Incoming>,
    pub redis: redis::aio::ConnectionManager,
    pub cache: Cache<String, Bytes>,
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
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Box<dyn std::error::Error + Send + Sync>> {
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
        .body(
            Full::new(Bytes::from(stats_html))
            .map_err(|e| match e {})
            .boxed()
        )
        .unwrap()
    )
}

async fn lb_handler(
    mut req: Request<Incoming>,
    state: Arc<LbState>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Box<dyn std::error::Error + Send + Sync>> {
    
    let curr_config = state.config.get();
    let is_get_request = req.method() == hyper::Method::GET;

    let client_ip = req
        .extensions()
        .get::<std::net::SocketAddr>()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    if curr_config.feature_flags.enable_rate_limiting {
        let mut redis_conn = state.redis.clone();

        let rate_key = format!("rate_limit:{}", client_ip);
        let max_reqs = 100;
        let window_sec = 60;

        let script = Script::new(RATE_LIMIT_SCRIPT);
        let limit_result: Result<Vec<i64>, redis::RedisError> = script
            .key(&rate_key)
            .arg(max_reqs)
            .arg(window_sec)
            .invoke_async(&mut redis_conn)
            .await;

        //Fail-Open
        if let Ok(res) = limit_result {
            let count = res[0];
            if count > max_reqs {
                eprintln!("BlOCKING IP {}: Rate limit exceeded.", client_ip);
                return Ok(make_boxed_response(429, "429 Too Many Requests\n")); 
            }
        }
    }

    let path = req.uri().path().to_string();

    if curr_config.feature_flags.enable_caching && req.method() == hyper::Method::GET {
        if let Some(cached_bytes) = state.cache.get(&path).await {
            println!("CACHE HIT: {}", path);
            return Ok(make_boxed_response(200, cached_bytes));
        }
    }

    if path == "/stats"{
        return handle_stats_request(state).await;
    }

    let target_backend = match state.registry.least_connections() {
        Some(b) => b,
        None => {
            eprintln!("CRITICAL: No Healthy backends found!");
            return Ok(make_boxed_response(503, "No healthy backends avaiable\n"));
        }
    };

    target_backend.active_connections.fetch_add(1, Ordering::SeqCst);

    let target_backend_addr = target_backend.addr.clone();

    let uri_string = format!("http://{}{}", target_backend.addr, path);
    let uri = uri_string.parse::<hyper::Uri>()?;
    *req.uri_mut() = uri;

    req.headers_mut()
        .insert("X-Forwarded-For", client_ip.parse()?);

    let response_result = state.client.request(req).await;

    target_backend.active_connections.fetch_sub(1, Ordering::SeqCst);

    let response = match response_result{
        Ok(res) => {
            target_backend.consecutive_errors.store(0, Ordering::SeqCst);
            res
        },
        Err(e) => {
            let errors = target_backend.consecutive_errors.fetch_add(1, Ordering::SeqCst) + 1;
            eprintln!("Backend {} failed to respond: {}. (Error count: {})", target_backend_addr, e, errors);

            if errors >= 5 {
                eprintln!(" CIRCUIT BREAKER TRIPPED! Removing {} from active rotation.", target_backend_addr);
                target_backend.is_alive.store(false, Ordering::SeqCst);
            }

            return Ok(make_boxed_response(
                    502,
                    format!("502 Bad Gateway: Backend {} dropped the Connection\n", target_backend_addr)
            ));
        }
    };

    if response.status().is_success(){
        let mut redis_conn = state.redis.clone();
        let stats_key = format!("lb:hits:{}", target_backend_addr);

        tokio::spawn(async move{
            if let Err(e) = redis_conn.incr::<_, _, ()>(stats_key, 1).await{
                eprintln!("Redis Stats Error: {}", e);
        }
    });
}
    let is_cacheable = curr_config.feature_flags.enable_caching
    && is_get_request
    && response.status().is_success();

    if is_cacheable {
        let content_length = response
            .headers()
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|val| val.to_str().ok())
            .and_then(|val| val.parse::<usize>().ok())
            .unwrap_or(usize::MAX);
        
        let five_megabytes = 5 * 1024 * 1024;

        if content_length <= five_megabytes {
            println!(" CACHING RESPONSE: {} ({} bytes)", path, content_length);

            let (parts, body) = response.into_parts();
            let collected_bytes = body.collect().await?.to_bytes();

            state.cache.insert(path, collected_bytes.clone()).await;

            return Ok(Response::from_parts (
                    parts,
                    Full::new(collected_bytes).map_err(|e| match e {}).boxed()
            ));
        }
        else {
            println!(" FILE TOO LARGE TO CACHE: {}. Streaming instead.",path);
        }
    }

    Ok(response.map(|body| body.boxed()))
}

fn make_boxed_response<B: Into<Bytes>>(
    status: u16,
    body: B,
) -> Response<BoxBody<Bytes, hyper::Error>> {
    Response::builder()
        .status(status)
        .body(
            Full::new(body.into())
            .map_err(|e| match e {})
            .boxed(),
        )
        .unwrap()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Auth::parse();

    let config_manager = Arc::new(ConfigManager::new("config.toml")?);
    ConfigManager::watch_for_changes(Arc::clone(&config_manager))?;

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
                    active_connections: std::sync::atomic::AtomicUsize::new(0),
                    consecutive_errors: AtomicUsize::new(0),
                },
                Backend {
                    addr: "127.0.0.1:9002".to_string(),
                    is_alive: std::sync::atomic::AtomicBool::new(true),
                    active_connections: std::sync::atomic::AtomicUsize::new(0),
                    consecutive_errors: AtomicUsize::new(0),
                },
            ],
            curr: AtomicUsize::new(0),
        },
        config: config_manager,
        client: Client::builder(TokioExecutor::new()).build_http(),
        redis: redis_manager,
        cache: Cache::builder()
            .max_capacity(100 * 1024 * 1024)
            .time_to_live(Duration::from_secs(60))
            .build(),
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
                        backend.consecutive_errors.store(0, Ordering::SeqCst);
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
