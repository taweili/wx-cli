mod config;
mod ipc;
mod crypto;
mod scanner;
mod daemon;
mod cli;
pub mod transport;

fn main() {
    init_logging();
    if std::env::var("WX_DAEMON_MODE").is_ok() {
        daemon::run();
    } else {
        cli::run();
    }
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    // 默认只输出 WARN+ 到 stderr（不污染用户可见的 stdout）。
    // 通过 `RUST_LOG=info` 或 `RUST_LOG=debug` 开启详细日志。
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info")
    });
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .with_env_filter(env_filter)
        .init();
}
