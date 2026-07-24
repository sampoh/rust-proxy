use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "http_proxy", about = "Local HTTP proxy with HTTPS forwarding")]
pub struct Cli {
    /// Target base URL (e.g., https://example.com)
    #[arg(long)]
    pub target: String,

    /// Local listen address
    #[arg(long, default_value = "127.0.0.1:18080")]
    pub listen: String,

    /// Optional second target base URL. When set, a second proxy is started.
    #[arg(long)]
    pub target2: Option<String>,

    /// Optional second local listen address. Ignored unless --target2 is set.
    /// Defaults to the --listen port + 1 on the same host.
    #[arg(long)]
    pub listen2: Option<String>,

    /// Upstream request timeout in seconds (0 = no timeout)
    #[arg(long, default_value_t = 200)]
    pub timeout: u64,

    /// Upstream request timeout in seconds for --target2 (0 = no timeout).
    /// If omitted, --timeout is used.
    #[arg(long)]
    pub timeout2: Option<u64>,
}
