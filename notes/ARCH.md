# wx-cli Architecture Analysis

## Overview

**wx-cli** is a cross-platform Rust CLI tool for extracting and querying local WeChat 4.x data. It decrypts SQLCipher-encrypted databases, caches decrypted copies with mtime-aware invalidation, and provides a daemon-based IPC architecture for fast repeated queries.

**Key characteristics:**
- Single binary, zero runtime dependencies
- Cross-platform: macOS, Linux, Windows
- Millisecond response times via daemon caching
- AI-friendly output (YAML by default, JSON optional)
- All data processed locally, no network calls

---

## High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                          wx (CLI client)                             │
│  src/cli/mod.rs - clap-based command parsing                        │
│  Commands: init, sessions, history, search, contacts, export,       │
│            unread, members, new-messages, stats, favorites, sns-*   │
└────────────────────────────┬────────────────────────────────────────┘
                             │ IPC (Unix socket / Windows named pipe)
                             ▼
┌─────────────────────────────────────────────────────────────────────┐
│                      wx-daemon (background process)                  │
│  src/daemon/mod.rs - tokio async runtime                            │
│                                                                      │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────────────┐  │
│  │   DbCache    │    │    Names     │    │   IPC Server         │  │
│  │ (mtime-aware)│    │ (contact map)│    │ (JSON line protocol) │  │
│  │  src/daemon/ │    │  src/daemon/ │    │    src/daemon/       │  │
│  │    cache.rs  │    │    query.rs  │    │      server.rs       │  │
│  └──────────────┘    └──────────────┘    └──────────────────────┘  │
│                                                                      │
│  On startup:                                                         │
│  1. Load config + keys from ~/.wx-cli/                              │
│  2. Pre-warm: decrypt session.db, sns.db, load contacts             │
│  3. Listen on socket/pipe for requests                              │
└────────────────────────────┬────────────────────────────────────────┘
                             │
                             ▼
┌─────────────────────────────────────────────────────────────────────┐
│                         Crypto Layer                                 │
│  src/crypto/mod.rs + wal.rs                                          │
│                                                                      │
│  - SQLCipher 4 page decryption (AES-256-CBC)                        │
│  - WAL (Write-Ahead Log) application                                │
│  - Streaming decryption (page-by-page, avoids full-file load)       │
└────────────────────────────┬────────────────────────────────────────┘
                             │
                             ▼
┌─────────────────────────────────────────────────────────────────────┐
│                       Scanner Layer                                  │
│  src/scanner/{macos,linux,windows}.rs                                │
│                                                                      │
│  Platform-specific memory scanners:                                  │
│  - macOS: Mach VM API (task_for_pid, mach_vm_region, mach_vm_read)  │
│  - Linux: /proc/<pid>/mem + /proc/<pid>/maps                        │
│  - Windows: CreateToolhelp32Snapshot + ReadProcessMemory            │
│                                                                      │
│  Pattern: x'<64hex_key><32hex_salt>' in WeChat process memory       │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Module Breakdown

### 1. Entry Point (`src/main.rs`)

```rust
fn main() {
    if std::env::var("WX_DAEMON_MODE").is_ok() {
        daemon::run();  // Background daemon mode
    } else {
        cli::run();     // CLI client mode
    }
}
```

Single binary acts as both client and daemon. Daemon spawned via `WX_DAEMON_MODE=1` env var.

---

### 2. CLI Layer (`src/cli/`)

**`mod.rs`** - Command definitions via clap derive macros:
- 17 subcommands (Init, Sessions, History, Search, Contacts, Export, Unread, Members, NewMessages, Stats, Favorites, SnsNotifications, SnsFeed, SnsSearch, Daemon)
- Each command dispatches to dedicated module (e.g., `history::cmd_history`)
- All commands share `--json` flag for output format toggle

**`transport.rs`** - IPC client:
- `ensure_daemon()` - auto-start daemon if not running
- `send()` - JSON line protocol over Unix socket / Windows named pipe
- Timeout handling (15s startup, 120s request)
- Permission preflight check for ~/.wx-cli/ directory

**Command modules** (`sessions.rs`, `history.rs`, etc.):
- Parse CLI args → build IPC `Request`
- Send to daemon → receive `Response`
- Format output (YAML/JSON) via `output.rs`

---

### 3. Daemon Layer (`src/daemon/`)

**`mod.rs`** - Daemon lifecycle:
```rust
async fn async_run() -> Result<()> {
    // 1. Create ~/.wx-cli/ + cache/ directories
    // 2. Write PID file
    // 3. Setup signal handlers (SIGTERM/SIGINT)
    // 4. Load config + keys
    // 5. Initialize DbCache (mtime-aware decryption cache)
    // 6. Pre-warm: load contacts, decrypt session.db + sns.db
    // 7. Start IPC server (blocking loop)
}
```

**`cache.rs`** - DbCache (critical performance component):
- `HashMap<String, CacheEntry>` in-memory cache
- `CacheEntry`: `{ db_mtime, wal_mtime, decrypted_path }`
- **mtime-aware invalidation**: re-decrypt only when `.db` or `.db-wal` mtime changes
- Persistent mtime records in `~/.wx-cli/cache/_mtimes.json`
- Cache reuse on daemon restart (avoids re-decryption)
- Uses MD5 hash of rel_key for cache filename

**`server.rs`** - IPC server:
- Unix: `tokio::net::UnixListener` on `~/.wx-cli/daemon.sock`
- Windows: `interprocess` named pipe `\\.\pipe\wx-cli-daemon`
- One connection per request, JSON line protocol
- `dispatch()` routes `Request` → query functions

**`query.rs`** - Query implementations (~1500 lines):
- `Names` struct: contact name cache + MD5→username lookup + verify_flags
- `chat_type_of()`: classify as `private`/`group`/`official_account`/`folded`
- Query functions: `q_sessions`, `q_history`, `q_search`, `q_contacts`, `q_unread`, `q_members`, `q_new_messages`, `q_stats`, `q_favorites`, `q_sns_*`
- Message parsing: zstd decompression, XML extraction (appmsg, sysmsg, revokemsg)
- Uses `spawn_blocking` for SQLite queries (rusqlite is sync)

---

### 4. Crypto Layer (`src/crypto/`)

**`mod.rs`** - SQLCipher 4 decryption:
```rust
// Constants
PAGE_SZ = 4096
SALT_SZ = 16
RESERVE_SZ = 80  // IV(16) + HMAC(64)

// Key operations
fn decrypt_page(enc_key: &[u8; 32], page_data: &[u8], pgno: u32) -> Vec<u8>
fn full_decrypt(db_path: &Path, out_path: &Path, enc_key: &[u8; 32])
```

**Algorithm:**
- AES-256-CBC decryption
- IV located at page end: `PAGE_SZ - RESERVE_SZ` offset
- Page 1 special handling: skip 16-byte SALT, write SQLite magic header
- Other pages: decrypt `[0..PAGE_SZ-RESERVE_SZ]`
- Streaming (page-by-page) to avoid full-file memory load

**`wal.rs`** - WAL application:
- WAL header: 32 bytes (magic, format, page_sz, ckpt_seq, salt1/2, cksum1/2)
- Frame: 24-byte header + PAGE_SZ data
- Frame matching via salt1/2 validation
- Random-write to decrypted DB at `(pgno-1) * PAGE_SZ`

---

### 5. Scanner Layer (`src/scanner/`)

**Common interface** (`mod.rs`):
```rust
pub struct KeyEntry {
    db_name: String,    // relative path
    enc_key: String,    // 64-char hex (32 bytes)
    salt: String,       // 32-char hex (16 bytes)
}

pub fn scan_keys(db_dir: &Path) -> Result<Vec<KeyEntry>>  // platform-specific
pub fn read_db_salt(path: &Path) -> Option<String>
pub fn collect_db_salts(db_dir: &Path) -> Vec<(String, String)>
```

**Pattern searched**: `x'<96 hex chars>'` = 64-char key + 32-char salt

**macOS** (`macos.rs`):
- `task_for_pid` → get Mach task port (requires root + ad-hoc signed WeChat)
- `mach_vm_region` → enumerate VM regions
- `mach_vm_read` → read 2MB chunks
- Filter: `VM_PROT_READ | VM_PROT_WRITE` regions only
- Deduplication by (key, salt) pair

**Linux** (`linux.rs`):
- `/proc/<pid>/comm` → find `wechat`/`weixin` process
- `/proc/<pid>/maps` → parse `rw-` regions
- `/proc/<pid>/mem` → seek + read
- Same chunk/dedup strategy

**Windows** (`windows.rs`):
- `CreateToolhelp32Snapshot` → find `Weixin.exe`
- `OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_INFORMATION)`
- `VirtualQueryEx` → enumerate `MEM_COMMIT + PAGE_READWRITE` regions
- `ReadProcessMemory` → chunk read

---

### 6. IPC Protocol (`src/ipc.rs`)

**Request** (tagged enum):
```rust
pub enum Request {
    Ping,
    Sessions { limit: usize },
    History { chat, limit, offset, since, until, msg_type },
    Search { keyword, chats, limit, since, until, msg_type },
    Contacts { query, limit },
    Unread { limit, filter },
    Members { chat },
    NewMessages { state, limit },
    Stats { chat, since, until },
    Favorites { limit, fav_type, query },
    SnsNotifications { limit, since, until, include_read },
    SnsFeed { limit, since, until, user },
    SnsSearch { keyword, limit, since, until, user },
}
```

**Response**:
```rust
pub struct Response {
    ok: bool,
    error: Option<String>,
    data: Value,  // flattened JSON
}
```

Protocol: newline-delimited JSON, one request per connection.

---

### 7. Config Layer (`src/config.rs`)

**Config struct**:
```rust
pub struct Config {
    db_dir: PathBuf,        // WeChat db_storage path
    keys_file: PathBuf,     // all_keys.json
    decrypted_dir: PathBuf, // (unused, cache dir used instead)
    wechat_process: String, // process name for scanner
}
```

**Paths**:
- `cli_dir()`: `~/.wx-cli/`
- `sock_path()`: `~/.wx-cli/daemon.sock`
- `cache_dir()`: `~/.wx-cli/cache/`
- `mtime_file()`: `~/.wx-cli/cache/_mtimes.json`

**Auto-detection** (`auto_detect_db_dir()`):
- macOS: `~/Library/Containers/com.tencent.xinWeChat/.../xwechat_files/*/db_storage`
- Linux: `~/Documents/xwechat_files/*/db_storage` + legacy path
- Windows: `%APPDATA%/Tencent/xwechat/config/*.ini` → parse data root

---

## Data Flow

### Init Flow (`wx init`)

```
1. Auto-detect db_dir → scan for db_storage directory
2. collect_db_salts(db_dir) → (salt_hex, rel_path) list
3. scan_keys(db_dir) → memory scan → (key_hex, salt_hex) candidates
4. Match: salt_hex == db_salt → KeyEntry { db_name, enc_key, salt }
5. Write ~/.wx-cli/config.json + ~/.wx-cli/all_keys.json
```

### Query Flow (e.g., `wx history "张三"`)

```
1. CLI: parse args → Request::History { chat: "张三", limit: 50 }
2. transport::ensure_daemon() → start if not alive
3. transport::send(Request) → Unix socket/pipe → daemon
4. daemon::dispatch(Request) → q_history()
   a. resolve_username("张三") → "wxid_xxx" (fuzzy match against Names)
   b. find_msg_tables(db, names, username) → [(db_path, "Msg_<md5>")]
   c. spawn_blocking: SQLite query on decrypted db_path
   d. decompress_message (zstd) + fmt_content (XML parsing)
5. Response::ok(json!{ chat, messages, ... })
6. CLI: output.rs → YAML/JSON formatting
```

### Decryption Flow (DbCache::get)

```
1. Check in-memory cache: if entry.mtime matches → return cached path
2. mtime mismatch or missing → spawn_blocking decrypt:
   a. crypto::full_decrypt(db_path, out_path, enc_key)
   b. If .db-wal exists: wal::apply_wal(wal_path, out_path, enc_key)
3. Update cache entry + persist mtimes to _mtimes.json
4. Return decrypted path for SQLite query
```

---

## Database Schema Knowledge

**session/session.db**:
- `SessionTable`: username, unread_count, summary, last_timestamp, last_msg_type, last_msg_sender

**contact/contact.db**:
- `contact`: username, nick_name, remark, verify_flag
- `chat_room`: id, owner (for group info)
- `chatroom_member`: room_id, member_id (joined with contact)

**message/message_N.db**:
- `Msg_<md5(username)>`: local_id, local_type, create_time, real_sender_id, message_content, WCDB_CT_message_content
- `Name2Id`: rowid → user_name (sender lookup)
- WCDB_CT = 4 means zstd compression

**sns/sns.db**:
- `sns_notification`: type (like/comment), from_nickname, content, feed_preview
- `sns_feed_xml`: author, contentDesc, media XML, createTime

**favorite/favorite.db**:
- `fav_db_item`: local_id, type, update_time, content, fromusr

---

## Performance Optimizations

1. **mtime-aware caching**: Only re-decrypt when source file changes
2. **Pre-warming**: Decrypt session.db + sns.db + contacts on daemon start
3. **Arc-wrapped Names**: Contact cache shared via Arc, cloned in O(1)
4. **spawn_blocking**: Sync SQLite ops off async runtime
5. **Streaming decrypt**: Page-by-page, no full file in memory
6. **WAL handling**: Apply uncommitted writes without re-decrypt
7. **MD5 table lookup**: `Msg_<md5>` → username via precomputed hash map

---

## Security Considerations

1. **Root/Admin required**: Memory scan needs elevated privileges
2. **No secrets logged**: Keys written to file, never echoed
3. **Socket permissions**: Unix socket mode 0600
4. **Local-only**: All IPC is localhost, no network exposure
5. **User consent implied**: Only decrypts own WeChat data

---

## Error Handling Patterns

- `anyhow::Result` throughout
- Context messages for chain debugging
- Graceful degradation: missing tables → fallback paths
- Preflight checks (e.g., ~/.wx-cli writable before daemon spawn)
- Signal handlers for clean shutdown (socket/PID file cleanup)

---

## Cross-Platform Notes

| Platform | Scanner API | IPC | Privilege | DB Path |
|----------|-------------|-----|-----------|---------|
| macOS | Mach VM | Unix socket | sudo + codesign | ~/Library/Containers/... |
| Linux | /proc/pid/mem | Unix socket | sudo | ~/Documents/xwechat_files |
| Windows | ToolHelp + ReadProcessMemory | Named pipe | Admin | %APPDATA%/Tencent/xwechat |

---

## Testing Coverage

- `src/crypto/mod.rs`: hex encoding, salt reading, recursive collection
- `src/scanner/macos.rs`: pattern matching (uppercase, dedup, embedded, edge cases)
- Unit tests for helper functions; integration tests would require live WeChat

---

## Extension Points

1. **New commands**: Add to `cli/mod.rs` enum + dispatch + query.rs function
2. **New message types**: Extend `fmt_type()` + `fmt_content()` parsers
3. **New DB sources**: Add to DbCache key list + query functions
4. **Output formats**: Extend `output.rs` formatter

---

## File Structure Summary

```
src/
├── main.rs           # Entry point (daemon/CLI switch)
├── config.rs         # Config loading + auto-detect
├── ipc.rs            # Request/Response protocol types
├── cli/
│   ├── mod.rs        # clap command definitions + dispatch
│   ├── transport.rs  # IPC client + daemon lifecycle
│   ├── output.rs     # YAML/JSON formatting
│   ├── init.rs       # wx init implementation
│   ├── sessions.rs   # etc. (thin wrappers around IPC)
│   └── daemon_cmd.rs # daemon status/stop/logs
├── daemon/
│   ├── mod.rs        # daemon entry + async_run
│   ├── cache.rs      # DbCache (mtime-aware decryption cache)
│   ├── server.rs     # IPC server (Unix/Windows)
│   └── query.rs      # All query implementations
├── crypto/
│   ├── mod.rs        # SQLCipher page decryption
│   └── wal.rs        # WAL application
└── scanner/
    ├── mod.rs        # common interface + salt collection
    ├── macos.rs      # Mach VM memory scanner
    ├── linux.rs      # /proc scanner
    └── windows.rs    # Windows API scanner
```

---

## Dependencies

**Core crates:**
- `clap` (derive) - CLI parsing
- `tokio` (full) - async runtime
- `serde`/`serde_json` - serialization
- `rusqlite` (bundled) - SQLite queries
- `aes`/`cbc`/`hmac`/`sha2`/`pbkdf2` - crypto primitives
- `zstd` - message decompression
- `chrono` - timestamp formatting
- `anyhow` - error handling
- `dirs` - home directory
- `md5` - table name hashing
- `regex` - Msg_<md5> pattern matching

**Platform-specific:**
- Unix: `libc` (setsid, signal handling)
- Windows: `windows` crate (process/memory APIs), `interprocess` (named pipes)

---

## Summary

wx-cli is a well-architected Rust project demonstrating:
- Clean separation of CLI/daemon/crypto/scanner layers
- Async-first daemon with sync-offload for SQLite
- Smart caching strategy (mtime-based invalidation)
- Cross-platform memory scanning for SQLCipher key extraction
- AI-friendly output design (YAML default, JSON optional)
- Comprehensive command coverage for WeChat local data