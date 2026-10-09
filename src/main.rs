mod app;
mod config;
mod palette;
mod ui;

use clap::Parser;

#[derive(Parser)]
#[command(name = "pixdrawd", about = "Multiplayer terminal pixel-art over SSH")]
struct Args {
    /// Address to listen on. `::` binds both IPv6 and (mapped) IPv4.
    #[arg(long, default_value = "::")]
    bind: String,

    /// Port to listen on. Use something other than 22 so it does not fight the
    /// host's real sshd.
    #[arg(long, default_value_t = 2222)]
    port: u16,

    /// Path to the persistent SSH host key (created on first run).
    #[arg(long, default_value = "host_key")]
    host_key: String,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = config::load();
    ui::init_from_config(&cfg.theme);
    println!(
        "pixdrawd would listen on {}:{} (host key: {}) with {} palette colours",
        args.bind,
        args.port,
        args.host_key,
        cfg.palette.colors.len(),
    );
    Ok(())
}
