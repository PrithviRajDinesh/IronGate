use clap::Parser;
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

pub struct Backend {
    pub addr: String,
}

pub struct BackendRegistry {
    pub backends: Vec<Backend>,
    pub curr: AtomicUsize,
}

impl BackendRegistry {
    pub fn round_robin(&self) -> &Backend {
        let ind = self.curr.fetch_add(1, Ordering::SeqCst) % self.backends.len();
        &self.backends[ind]
    }
}

pub struct LbState {
    pub registry: BackendRegistry,
    pub client: Client<hyper_util::client::legacy::connect::HttpConnector, Incoming>,
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

async fn lb_handler(
    mut req: Request<Incoming>,
    state: Arc<LbState>,
) -> Result<Response<Incoming>, Box<dyn std::error::Error + Send + Sync>> {
    let path = req.uri().path();
    let target_backend = state.registry.round_robin();

    let uri_string = format!("{}{}", target_backend.addr, path);
    let uri = uri_string.parse::<hyper::Uri>()?;
    *req.uri_mut() = uri;

    let response = state.client.request(req).await?;
    Ok(response)
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Auth::parse();

    let state = Arc::new(LbState {
        registry: BackendRegistry {
            backends: vec![
                Backend { addr: "http://127.0.0.1:9001".to_string() },
                Backend { addr: "http://127.0.0.1:9002".to_string() },
            ],
            curr: AtomicUsize::new(0),
        },
        client: Client::builder(TokioExecutor::new()).build_http(),
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

    loop {
        let (raw_socket, _peer_addr) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let state_for_task = Arc::clone(&state); // Clone for the async task

        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(raw_socket).await {
                Ok(stream) => stream,
                Err(e) => {
                    eprintln!("Handshake failed: {}", e);
                    return;
                }
            };

            let io = TokioIo::new(tls_stream);

            let svc = tower::service_fn(move |req| {
                lb_handler(req, Arc::clone(&state_for_task))
            });
            let logger_svc = Logger::new(svc);
            let final_svc = TowerToHyperService::new(logger_svc);

            if let Err(err) = http1::Builder::new().serve_connection(io, final_svc).await {
                eprintln!("Connection error: {}", err);
            }
        });
    }
}
