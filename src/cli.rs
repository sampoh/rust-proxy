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

    /// Upstream request timeout in seconds (0 = no timeout)
    #[arg(long, default_value_t = 200)]
    pub timeout: u64,
}
