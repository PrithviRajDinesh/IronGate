# ⚡ Tachyon

A high-performance, production-grade load balancer written in Rust. Tachyon routes HTTP traffic across multiple backend servers with TLS termination, real-time metrics, intelligent load balancing, and live hot-reload configuration.

---

## Features

- **Multiple load balancing algorithms** — weighted random, round-robin, least connections, and IP hash
- **TLS termination** — HTTPS with self-signed or CA-issued certificates via `rustls`
- **Circuit breaker** — automatically removes backends that fail 5 consecutive requests
- **Health checks** — background TCP health checks every 5 seconds
- **Hot-reload config** — edit `config.toml` and changes apply instantly without restart
- **Redis-backed rate limiting** — per-IP sliding window rate limiting via Lua scripts
- **Response caching** — in-memory LRU cache with configurable TTL (up to 100MB)
- **Prometheus metrics** — exposes `/metrics` with request counts and active connections per backend
- **Live dashboard** — browser-based real-time monitoring dashboard
- **Graceful shutdown** — drains active connections before exiting on `Ctrl+C`

---

## Architecture

```
Client (HTTPS)
      │
      ▼
  Tachyon (TLS Termination)
      │
      ├── /metrics → Prometheus text format
      ├── /stats   → Redis hit counts
      │
      ├── Rate Limiter (Redis)
      ├── Cache (Moka)
      │
      ▼
  Backend Registry
      ├── Weighted Random
      ├── Round Robin
      ├── Least Connections
      └── IP Hash
            │
            ▼
    Backend Servers (HTTP)
```

---

## Getting Started

### Prerequisites

- Rust (latest stable)
- Redis
- TLS certificate and key (see below)

### Generate a self-signed certificate

```bash
openssl req -x509 -newkey rsa:4096 -keyout localhost-key.pem \
  -out localhost.pem -days 365 -nodes -subj '/CN=localhost'
```

### Configure backends

Edit `config.toml`:

```toml
server_port = 8080
timeout_ms = 8000
max_connections = 1000

[[backends]]
address = "127.0.0.1:9001"
weight = 10

[[backends]]
address = "127.0.0.1:9002"
weight = 1

[feature_flags]
enable_caching = false
enable_rate_limiting = false
rate_limit_per_second = 100
```

### Run

```bash
cargo run -- --cert localhost.pem --key localhost-key.pem --addr 0.0.0.0:8080
```

---

## Docker

### Run with Docker Compose

```bash
docker compose up --build
```

This starts Tachyon and a Redis instance. The load balancer is available at `https://localhost:8080`.

### docker-compose.yml

```yaml
services:
  tachyon:
    build: .
    ports:
      - "8080:8080"
    depends_on:
      redis:
        condition: service_healthy
    volumes:
      - ./config.toml:/app/config.toml

  redis:
    image: redis:7-alpine
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]
      interval: 5s
      timeout: 3s
      retries: 5
```

---

## Live Dashboard

Tachyon ships with a browser-based live dashboard (`dashboard.html`) that connects directly to the `/metrics` endpoint.

**To use it:**

```bash
# Serve the dashboard
python3 -m http.server 3000

# Open in browser
http://localhost:3000/dashboard.html
```

The dashboard shows total requests, active connections, req/sec, error rate, per-backend traffic share, and a live sparkline chart.

---

## Metrics

Tachyon exposes Prometheus-compatible metrics at `/metrics`:

| Metric | Type | Description |
|--------|------|-------------|
| `tachyon_requests_total` | Counter | Total requests routed, labeled by backend and status |
| `tachyon_active_connections` | Gauge | Currently active connections per backend |

```bash
curl -k https://localhost:8080/metrics
```

---

## Configuration Reference

| Key | Default | Description |
|-----|---------|-------------|
| `server_port` | `8080` | Port to listen on |
| `timeout_ms` | `8000` | Request timeout in milliseconds |
| `max_connections` | `1000` | Maximum concurrent connections |
| `enable_caching` | `false` | Enable in-memory response cache |
| `enable_rate_limiting` | `false` | Enable per-IP rate limiting |
| `rate_limit_per_second` | `100` | Max requests per IP per second |

Configuration changes are picked up automatically — no restart required.

---

## Testing

Start test backends:

```bash
python3 -m http.server 9001 &
python3 -m http.server 9002 &
```

Generate traffic:

```bash
while true; do curl -k -s https://127.0.0.1:8080/ > /dev/null; sleep 0.1; done
```

Test circuit breaker by killing a backend:

```bash
kill %1  # Iron Gate removes it after 5 errors
```

---

## Tech Stack

| Component | Crate |
|-----------|-------|
| HTTP server/client | `hyper` + `hyper-util` |
| TLS | `rustls` + `tokio-rustls` |
| Async runtime | `tokio` |
| Middleware | `tower` |
| Cache | `moka` |
| Redis | `redis` |
| Metrics | `prometheus` |
| Config | `toml` + `arc-swap` + `notify` |
| CLI | `clap` |

---

## License

MIT
