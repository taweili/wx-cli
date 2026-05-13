# Communication Layer Analysis & TCP Socket Proposal

## Current Communication Architecture

### Layer Overview

```
┌─────────────────────────────────────────────────────────────┐
│                    Protocol Layer                            │
│  src/ipc.rs                                                  │
│  Request / Response types + JSON serialization               │
│  (Well abstracted - transport-agnostic)                      │
└────────────────────────────┬────────────────────────────────┘
                             │
┌────────────────────────────┴────────────────────────────────┐
│                    Server Layer                              │
│  src/daemon/server.rs                                        │
│  Platform-specific listeners + connection handlers           │
│  (POOR abstraction - duplicated logic per platform)          │
└────────────────────────────┬────────────────────────────────┘
                             │
┌────────────────────────────┴────────────────────────────────┐
│                    Client Layer                              │
│  src/cli/transport.rs                                        │
│  Platform-specific connection + send functions               │
│  (POOR abstraction - duplicated logic per platform)          │
└─────────────────────────────────────────────────────────────┘
```

---

## Abstraction Assessment

### Protocol Layer (src/ipc.rs) — **HIGH abstraction**

**Strengths:**
- Pure data types with serde derive
- No transport-specific code
- Clean API: `Request` enum, `Response` struct
- `to_json_line()` helper for serialization
- Transport-agnostic by design

**Example:**
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Sessions { limit: usize },
    History { chat: String, limit: usize, ... },
    // ... all commands
}

pub struct Response {
    pub ok: bool,
    pub error: Option<String>,
    #[serde(flatten)]
    pub data: Value,
}
```

**Verdict:** This layer is well-designed and TCP-ready. No changes needed.

---

### Server Layer (src/daemon/server.rs) — **LOW abstraction**

**Current structure:**
```rust
// Top-level entry with #[cfg] branching
pub async fn serve(db, names) -> Result<()> {
    #[cfg(unix)]
    serve_unix(db, names).await?;
    #[cfg(windows)]
    serve_windows(db, names).await?;
}

// Unix implementation (40 lines)
#[cfg(unix)]
async fn serve_unix(db, names) -> Result<()> {
    let listener = UnixListener::bind(&sock_path)?;
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async { handle_connection_unix(stream, db, names) });
    }
}

#[cfg(unix)]
async fn handle_connection_unix(stream, db, names) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let line = lines.next_line().await?;
    let req: Request = serde_json::from_str(&line)?;
    let resp = dispatch(req, &db, &names).await;
    writer.write_all(resp.to_json_line()?.as_bytes()).await?;
}

// Windows implementation (40 lines) - SAME LOGIC, DIFFERENT TYPES
#[cfg(windows)]
async fn serve_windows(db, names) -> Result<()> {
    let listener = ListenerOptions::new().name(name).create_tokio()?;
    loop {
        let conn = listener.accept().await?;
        tokio::spawn(async { handle_connection_windows(conn, db, names) });
    }
}

#[cfg(windows)]
async fn handle_connection_windows(conn, db, names) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(conn);
    let mut lines = BufReader::new(reader).lines();
    let line = lines.next_line().await?;
    let req: Request = serde_json::from_str(&line)?;
    let resp = dispatch(req, &db, &names).await;
    writer.write_all(resp.to_json_line()?.as_bytes()).await?;
}
```

**Problems:**
1. **Duplicated connection handling**: `handle_connection_unix` and `handle_connection_windows` have identical logic
2. **No abstraction for stream types**: `UnixStream` vs `interprocess::Stream` handled separately
3. **No abstraction for listener types**: `UnixListener` vs `interprocess::Listener` handled separately
4. **#[cfg] branching at function level**: Makes extension difficult
5. **`dispatch()` is shared but buried**: Good pattern, but underutilized

**Duplication count:** ~30 lines of identical JSON-line protocol handling duplicated per platform

---

### Client Layer (src/cli/transport.rs) — **LOW abstraction**

**Current structure:**
```rust
// is_alive() with #[cfg] branching
pub fn is_alive() -> bool {
    #[cfg(unix)]
    {
        let stream = UnixStream::connect(&sock_path)?;
        // ping logic
    }
    #[cfg(windows)]
    {
        let stream = Stream::connect(name)?;
        // ping logic (different API)
    }
}

// send() with #[cfg] branching
pub fn send(req: Request) -> Result<Response> {
    ensure_daemon()?;
    #[cfg(unix)]
    { send_unix(req) }
    #[cfg(windows)]
    { send_windows(req) }
}

#[cfg(unix)]
fn send_unix(req: Request) -> Result<Response> {
    let stream = UnixStream::connect(&sock_path)?;
    stream.write_all(serde_json::to_string(&req)? + "\n");
    let line = BufReader::new(&stream).read_line();
    let resp: Response = serde_json::from_str(&line)?;
    Ok(resp)
}

#[cfg(windows)]
fn send_windows(req: Request) -> Result<Response> {
    let stream = Stream::connect(name)?;
    stream.write_all(serde_json::to_string(&req)? + "\n");
    let line = BufReader::new(stream).read_line();
    let resp: Response = serde_json::from_str(&line)?;
    Ok(resp)
}
```

**Problems:**
1. **Duplicated request/response handling**: Same JSON-line protocol, different stream types
2. **No abstraction for stream type**: Each platform uses different types
3. **`is_alive()` logic differs**: Windows version doesn't do full ping
4. **#[cfg] branching scattered**: 3 separate locations

**Duplication count:** ~20 lines of identical protocol handling duplicated per platform

---

## Abstraction Score Summary

| Layer          | Abstraction Level | Duplicated Lines | Extension Difficulty |
|----------------|-------------------|------------------|---------------------|
| Protocol       | HIGH              | 0                | Easy                |
| Server         | LOW               | ~30              | Hard                |
| Client         | LOW               | ~20              | Hard                |

**Total duplicated code:** ~50 lines of identical JSON-line protocol handling

**Root cause:** No trait abstraction for `Listener` and `Connection` types

---

## Proposed Architecture for TCP Support

### Strategy: Trait-Based Abstraction

Introduce traits for transport primitives, implement for:
1. Unix socket (existing)
2. Windows named pipe (existing)
3. TCP socket (new)

---

### New Trait Definitions

```rust
// src/transport/traits.rs

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite};

/// Trait for accepting connections (server-side)
pub trait Listener: Send + Sync {
    type Connection: AsyncRead + AsyncWrite + Send + Sync + 'static;
    
    async fn accept(&self) -> Result<Self::Connection>;
    fn addr_desc(&self) -> String;  // for logging
}

/// Trait for connecting to server (client-side)
pub trait Connector: Send + Sync {
    type Connection: AsyncRead + AsyncWrite + Send + Sync + 'static;
    
    async fn connect(&self) -> Result<Self::Connection>;
    fn is_available(&self) -> bool;  // quick check before connect
}
```

---

### New Module Structure

```
src/transport/
├── mod.rs           # Public API: send(), handle_connection()
├── traits.rs        # Listener + Connector traits
├── unix.rs          # UnixListener + UnixConnector
├── windows.rs       # PipeListener + PipeConnector
├── tcp.rs           # TcpListener + TcpConnector
└── protocol.rs      # JSON-line protocol handling (shared)
```

**Key change:** Protocol handling moves to `protocol.rs`, shared by all transports

---

### Protocol Handler (Shared Code)

```rust
// src/transport/protocol.rs

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite, AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::ipc::{Request, Response};

/// Handle a single connection (server-side)
pub async fn handle_connection<C: AsyncRead + AsyncWrite + Unpin>(
    conn: C,
    db: Arc<DbCache>,
    names: Arc<tokio::sync::RwLock<Arc<Names>>>,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(conn);
    let mut lines = BufReader::new(reader).lines();
    
    let line = match lines.next_line().await? {
        Some(l) => l,
        None => return Ok(()),
    };
    
    let req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            let resp = Response::err(format!("JSON parse error: {}", e));
            writer.write_all(resp.to_json_line()?.as_bytes()).await?;
            return Ok(());
        }
    };
    
    let resp = dispatch(req, db, names).await;
    writer.write_all(resp.to_json_line()?.as_bytes()).await?;
    Ok(())
}

/// Send request and receive response (client-side)
pub async fn send_over_connection<C: AsyncRead + AsyncWrite + Unpin>(
    conn: C,
    req: &Request,
) -> Result<Response> {
    let (reader, mut writer) = tokio::io::split(conn);
    
    let req_str = serde_json::to_string(req)? + "\n";
    writer.write_all(req_str.as_bytes()).await?;
    
    let mut lines = BufReader::new(reader).lines();
    let line = lines.next_line().await?
        .ok_or_else(|| anyhow::anyhow!("No response received"))?;
    
    let resp: Response = serde_json::from_str(&line)?;
    if !resp.ok {
        anyhow::bail!("{}", resp.error.as_deref().unwrap_or("Unknown error"));
    }
    Ok(resp)
}
```

**This eliminates all 50 lines of duplication.**

---

### Unix Socket Implementation

```rust
// src/transport/unix.rs

use anyhow::Result;
use tokio::net::{UnixListener, UnixStream};

use super::traits::{Listener, Connector};

pub struct UnixSocketListener {
    listener: UnixListener,
    path: std::path::PathBuf,
}

impl Listener for UnixSocketListener {
    type Connection = UnixStream;
    
    async fn accept(&self) -> Result<Self::Connection> {
        let (stream, _) = self.listener.accept().await?;
        Ok(stream)
    }
    
    fn addr_desc(&self) -> String {
        self.path.display().to_string()
    }
}

pub struct UnixSocketConnector {
    path: std::path::PathBuf,
}

impl Connector for UnixSocketConnector {
    type Connection = UnixStream;
    
    async fn connect(&self) -> Result<Self::Connection> {
        UnixStream::connect(&self.path).await?
    }
    
    fn is_available(&self) -> bool {
        self.path.exists()
    }
}

// Factory functions
pub fn create_listener(path: &std::path::Path) -> Result<UnixSocketListener> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(UnixSocketListener { listener, path: path.to_owned() })
}

pub fn connector(path: &std::path::Path) -> UnixSocketConnector {
    UnixSocketConnector { path: path.to_owned() }
}
```

---

### Windows Named Pipe Implementation

```rust
// src/transport/windows.rs

use anyhow::Result;
use interprocess::local_socket::{
    tokio::prelude::*,
    GenericNamespaced, ListenerOptions,
};

use super::traits::{Listener, Connector};

pub struct PipeListener {
    listener: interprocess::local_socket::tokio::Listener,
    name: String,
}

impl Listener for PipeListener {
    type Connection = interprocess::local_socket::tokio::Stream;
    
    async fn accept(&self) -> Result<Self::Connection> {
        self.listener.accept().await?
    }
    
    fn addr_desc(&self) -> String {
        format!("\\\\.\\pipe\\{}", self.name)
    }
}

pub struct PipeConnector {
    name: String,
}

impl Connector for PipeConnector {
    type Connection = interprocess::local_socket::tokio::Stream;
    
    async fn connect(&self) -> Result<Self::Connection> {
        let ns_name = self.name.to_ns_name::<GenericNamespaced>()?;
        Stream::connect(ns_name).await?
    }
    
    fn is_available(&self) -> bool {
        // Windows named pipes don't have filesystem presence
        // Try a quick connect to check
        self.connect().await.is_ok()
    }
}

pub fn create_listener(name: &str) -> Result<PipeListener> {
    let ns_name = name.to_ns_name::<GenericNamespaced>()?;
    let listener = ListenerOptions::new().name(ns_name).create_tokio()?;
    Ok(PipeListener { listener, name: name.to_owned() })
}

pub fn connector(name: &str) -> PipeConnector {
    PipeConnector { name: name.to_owned() }
}
```

---

### TCP Socket Implementation (NEW)

```rust
// src/transport/tcp.rs

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};

use super::traits::{Listener, Connector};

pub struct TcpSocketListener {
    listener: TcpListener,
    addr: std::net::SocketAddr,
}

impl Listener for TcpSocketListener {
    type Connection = TcpStream;
    
    async fn accept(&self) -> Result<Self::Connection> {
        let (stream, addr) = self.listener.accept().await?;
        eprintln!("[tcp] connection from {}", addr);
        Ok(stream)
    }
    
    fn addr_desc(&self) -> String {
        self.addr.to_string()
    }
}

pub struct TcpSocketConnector {
    addr: std::net::SocketAddr,
}

impl Connector for TcpSocketConnector {
    type Connection = TcpStream;
    
    async fn connect(&self) -> Result<Self::Connection> {
        TcpStream::connect(&self.addr).await?
    }
    
    fn is_available(&self) -> bool {
        // TCP port check - try quick connect
        std::net::TcpStream::connect_timeout(&self.addr, std::time::Duration::from_millis(100)).is_ok()
    }
}

pub async fn create_listener(bind: &str) -> Result<TcpSocketListener> {
    let listener = TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    Ok(TcpSocketListener { listener, addr })
}

pub fn connector(addr: std::net::SocketAddr) -> TcpSocketConnector {
    TcpSocketConnector { addr }
}
```

---

### Server Refactor (src/daemon/server.rs)

```rust
// src/daemon/server.rs

use std::sync::Arc;
use crate::transport::{Listener, handle_connection};

pub async fn serve(
    db: Arc<DbCache>,
    names: Arc<tokio::sync::RwLock<Arc<Names>>>,
) -> Result<()> {
    // Determine transport based on config/env
    let listeners: Vec<Box<dyn Listener>> = build_listeners()?;
    
    for listener in listeners {
        eprintln!("[server] listening on {}", listener.addr_desc());
        let db2 = Arc::clone(&db);
        let names2 = Arc::clone(&names);
        
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok(conn) => {
                        let db3 = Arc::clone(&db2);
                        let names3 = Arc::clone(&names2);
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(conn, db3, names3).await {
                                eprintln!("[server] connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => eprintln!("[server] accept error: {}", e),
                }
            }
        });
    }
    
    // Keep daemon alive
    tokio::signal::ctrl_c().await?;
    Ok(())
}

fn build_listeners() -> Result<Vec<Box<dyn Listener>>> {
    let mut listeners = Vec::new();
    
    // Always add local transport (Unix/Pipe)
    #[cfg(unix)]
    listeners.push(Box::new(
        crate::transport::unix::create_listener(&crate::config::sock_path())?
    ));
    
    #[cfg(windows)]
    listeners.push(Box::new(
        crate::transport::windows::create_listener("wx-cli-daemon")?
    ));
    
    // Optionally add TCP (if configured)
    if let Ok(tcp_bind) = std::env::var("WX_TCP_BIND") {
        let tcp_listener = crate::transport::tcp::create_listener(&tcp_bind).await?;
        eprintln!("[server] TCP enabled on {}", tcp_listener.addr_desc());
        listeners.push(Box::new(tcp_listener));
    }
    
    Ok(listeners)
}
```

**Key changes:**
1. Single `serve()` function, no #[cfg] branching
2. `build_listeners()` constructs appropriate transport(s)
3. Can listen on multiple transports simultaneously (local + TCP)
4. `handle_connection()` from `transport::protocol` is shared

---

### Client Refactor (src/cli/transport.rs)

```rust
// src/cli/transport.rs (renamed to src/transport/mod.rs)

use anyhow::Result;
use crate::ipc::{Request, Response};
use crate::transport::{Connector, send_over_connection};

pub async fn send(req: Request) -> Result<Response> {
    ensure_daemon()?;
    
    // Try connectors in priority order
    let connectors = build_connectors();
    
    for connector in connectors {
        if connector.is_available() {
            let conn = connector.connect().await?;
            return send_over_connection(conn, &req).await;
        }
    }
    
    anyhow::bail!("No available transport to daemon")
}

fn build_connectors() -> Vec<Box<dyn Connector>> {
    let mut connectors = Vec::new();
    
    // Local transport first (faster, more secure)
    #[cfg(unix)]
    connectors.push(Box::new(
        crate::transport::unix::connector(&crate::config::sock_path())
    ));
    
    #[cfg(windows)]
    connectors.push(Box::new(
        crate::transport::windows::connector("wx-cli-daemon")
    ));
    
    // TCP fallback (if configured)
    if let Ok(tcp_addr) = std::env::var("WX_TCP_ADDR") {
        if let Ok(addr) = tcp_addr.parse() {
            connectors.push(Box::new(
                crate::transport::tcp::connector(addr)
            ));
        }
    }
    
    connectors
}

pub fn ensure_daemon() -> Result<()> {
    // Try ping on each connector
    for connector in build_connectors() {
        if connector.is_available() {
            // Try quick ping
            if let Ok(conn) = connector.connect().await? {
                // Use blocking ping for startup check
                // ... (existing logic wrapped)
                return Ok(());
            }
        }
    }
    
    // No daemon found, start it
    start_daemon()?;
    
    // Wait for any connector to become available
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        for connector in build_connectors() {
            if connector.is_available() {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    
    anyhow::bail!("Daemon startup timeout")
}
```

**Key changes:**
1. Async `send()` using `send_over_connection()`
2. `build_connectors()` returns prioritized list
3. Fallback chain: Unix/Pipe → TCP
4. No #[cfg] branching in main logic

---

## Configuration for TCP

### Environment Variables

```bash
# Server: enable TCP listener
WX_TCP_BIND=127.0.0.1:9876    # bind address (default: none)
WX_TCP_BIND=0.0.0.0:9876      # allow external connections (security risk)

# Client: TCP fallback address
WX_TCP_ADDR=127.0.0.1:9876    # connect address
WX_TCP_ADDR=192.168.1.100:9876 # remote daemon
```

### Config File Extension

```json
// ~/.wx-cli/config.json
{
  "db_dir": "...",
  "keys_file": "...",
  "tcp": {
    "bind": "127.0.0.1:9876",    // optional
    "allow_remote": false        // security flag
  }
}
```

---

## Security Considerations for TCP

### Risks

1. **No encryption**: JSON-line protocol sent in plaintext
2. **No authentication**: Anyone can query WeChat data
3. **Data exposure**: Chat history, contacts, etc. visible to network

### Recommended Safeguards

```rust
// src/transport/tcp.rs

pub struct TcpSocketListener {
    listener: TcpListener,
    addr: SocketAddr,
    allowed_hosts: Vec<IpNet>,  // CIDR whitelist
}

impl Listener for TcpSocketListener {
    async fn accept(&self) -> Result<Self::Connection> {
        let (stream, addr) = self.listener.accept().await?;
        
        // Check source IP against whitelist
        let ip = addr.ip();
        if !self.allowed_hosts.iter().any(|net| net.contains(&ip)) {
            eprintln!("[tcp] rejected connection from {}", addr);
            return Err(anyhow::anyhow!("IP not in whitelist"));
        }
        
        Ok(stream)
    }
}

// Config
pub struct TcpConfig {
    bind: String,
    allow_remote: bool,
    allowed_hosts: Vec<String>,  // ["127.0.0.1/8", "192.168.1.0/24"]
}
```

### Authentication Proposal (Optional)

```rust
// Add to Request enum
pub enum Request {
    Auth { token: String },  // new
    Ping,
    Sessions { ... },
}

// Server checks token before processing
async fn dispatch(req: Request, db: &DbCache, names: &Names, auth: &AuthState) -> Response {
    if !auth.is_authenticated() && !req.is_auth_request() {
        return Response::err("Not authenticated");
    }
    // ... normal dispatch
}
```

---

## Implementation Roadmap

### Phase 1: Refactor Existing Code

1. Create `src/transport/` module
2. Define `Listener` and `Connector` traits
3. Move Unix/Pipe implementations to `unix.rs` / `windows.rs`
4. Extract protocol handling to `protocol.rs`
5. Refactor `server.rs` to use trait
6. Refactor `transport.rs` to use trait

**Effort:** ~4 hours
**Benefit:** Eliminate 50 lines duplication, cleaner architecture

### Phase 2: Add TCP Support

1. Create `tcp.rs` with `TcpSocketListener` / `TcpSocketConnector`
2. Update `build_listeners()` / `build_connectors()`
3. Add config parsing for TCP options
4. Add IP whitelist validation

**Effort:** ~2 hours
**Benefit:** TCP connectivity for remote clients

### Phase 3: Security Hardening

1. Add authentication token support
2. TLS wrapper option (tokio-rustls)
3. Connection logging/audit

**Effort:** ~3 hours
**Benefit:** Production-safe remote access

---

## Backwards Compatibility

- Local transport (Unix/Pipe) remains default
- TCP opt-in via config/env (not automatic)
- CLI unchanged (same commands)
- Protocol unchanged (same Request/Response types)

---

## Alternative: Zero-Change TCP Proxy

If refactoring is not desired, a simpler approach:

```bash
# Use socat/proxy to expose Unix socket over TCP
socat TCP-LISTEN:9876,reuseaddr,fork UNIX-CONNECT:/home/user/.wx-cli/daemon.sock
```

**Pros:** No code changes
**Cons:** Requires external tool, no IP filtering, less integrated

---

## Summary

| Aspect                    | Current State           | Proposed State              |
|---------------------------|-------------------------|-----------------------------|
| Protocol abstraction      | HIGH (good)             | HIGH (unchanged)            |
| Transport abstraction     | LOW (platform-specific) | HIGH (trait-based)          |
| Duplicated code           | ~50 lines               | 0 lines                     |
| Extension difficulty      | Hard                    | Easy                        |
| TCP support               | None                    | Full                        |
| Multi-listener support    | None                    | Yes (local + TCP)           |

**Recommended path:** Proceed with Phase 1 refactor, then Phase 2 TCP addition. Phase 3 security can follow based on use case.

---

## Code Impact Summary

| File                     | Change Type        | Lines Changed |
|--------------------------|--------------------|---------------|
| src/transport/mod.rs     | New                | ~60           |
| src/transport/traits.rs  | New                | ~20           |
| src/transport/protocol.rs| New (from existing)| ~40           |
| src/transport/unix.rs    | New (refactor)     | ~40           |
| src/transport/windows.rs | New (refactor)     | ~40           |
| src/transport/tcp.rs     | New                | ~50           |
| src/daemon/server.rs     | Refactor           | ~30 (from 90) |
| src/cli/transport.rs     | Delete (moved)     | 0             |
| src/ipc.rs               | Unchanged          | 0             |

**Net change:** +250 new lines, -90 old lines, -50 duplication = +110 total
**Complexity reduction:** Platform branching centralized, extension point clear