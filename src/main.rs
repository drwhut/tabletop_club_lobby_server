/*
tabletop_club_lobby_server
Copyright (c) 2024 Benjamin 'drwhut' Beddows.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
*/

use clap::Parser;
use std::path::PathBuf;
use tokio::fs;
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};
#[cfg(windows)]
use tokio::signal::windows;
use tokio::sync::broadcast;
use tokio_native_tls::native_tls::Identity;
use tracing::{error, info, warn};
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::{fmt, prelude::*};

/// Lobby server for the open-source multiplayer game Tabletop Club.
#[derive(Parser)]
#[command(version, author)]
struct CommandLineArguments {
    /// Path to the server configuration file.
    ///
    /// This file is periodically checked for changes.
    ///
    /// If the file does not exist, it is automatically created.
    #[arg(short, long = "config", default_value = "server.toml")]
    config_path: PathBuf,

    /// The port number to bind the server to.
    #[arg(short, long, default_value_t = 9080)]
    port: u16,

    /// Enables TLS encryption.
    ///
    /// If set, a certificate and private key must be provided.
    #[arg(
        short,
        long = "tls",
        requires = "certificate_path",
        requires = "private_key_path"
    )]
    tls_enabled: bool,

    /// Path to the PEM-encoded X509 certificate file.
    #[arg(short = 'x', long = "x509-certificate", requires = "tls_enabled")]
    certificate_path: Option<PathBuf>,

    /// Path to the PEM-encoded private key file.
    #[arg(short = 'k', long = "private-key", requires = "tls_enabled")]
    private_key_path: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    // Print spans and events to STDOUT in a nice format.
    // NOTE: We don't want to include the full 'tabletop_club_lobby_server'
    // target name in each log.
    let fmt_layer = fmt::layer().with_target(false);

    // Filter what spans and events get output based on the `RUST_LOG`
    // environment variable. By default, INFO, WARN, and ERROR are output.
    let filter_layer = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    // Using all of the layers we've made, build a tracing subscriber that takes
    // all of the spans and events that come in and parses them through the
    // various layers.
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .init();

    // Parse the command-line arguments.
    let args = CommandLineArguments::parse();

    // Create the TcpListener, and bind it to this machine's address with the
    // given port.
    let address = format!("127.0.0.1:{}", args.port);
    let tcp_listener = TcpListener::bind(&address).await?;

    // Check if we need to create a cryptographic identity for the server.
    let maybe_identity = if args.tls_enabled {
        // We can safely unwrap here, as these arguments must be present due to
        // the way we set up the command line argument parser.
        let cert_bytes = fs::read(args.certificate_path.unwrap()).await?;
        let key_bytes = fs::read(args.private_key_path.unwrap()).await?;

        info!("tls enabled, creating identity");
        match Identity::from_pkcs8(&cert_bytes, &key_bytes) {
            Ok(id) => Some(id),
            Err(e) => {
                error!(error = %e, "failed to create identity, tls disabled");
                None
            }
        }
    } else {
        warn!("tls disabled, only modified clients can connect");
        None
    };

    // Among the arguments is the path to the configuration file - we need to
    // check if there is even a file there, and if there isn't, we need to
    // create it.
    let config_file_exists = fs::try_exists(&args.config_path).await?;
    let config_path_str = args.config_path.to_string_lossy().into_owned();
    if config_file_exists {
        info!(path = config_path_str, "using config file");
    } else {
        warn!(
            path = config_path_str,
            "config file does not exist, attempting to create it"
        );

        let default_config = tabletop_club_lobby_server::config::VariableConfig::default();
        default_config.write_config_file(&args.config_path).await?;
        info!(path = config_path_str, "config file created");
    }

    // Create a broadcast channel so that we can inform all of the spawned tasks
    // that we want them to shut down.
    let (shutdown_sender, shutdown_receiver) = broadcast::channel(1);

    // Create the context for the server, which contains everything it needs to
    // function properly.
    let server_context = tabletop_club_lobby_server::ServerContext {
        config_file_path: args.config_path,
        tcp_listener: tcp_listener,
        tls_identity: maybe_identity,
        shutdown_signal: shutdown_receiver,
    };

    // Start the server by spawning it's task!
    let mut server = tabletop_club_lobby_server::Server::spawn(server_context);

    // Now that we've started the server, we should wait for any signals from
    // the OS to terminate the process, so that we can shutdown gracefully.
    #[cfg(unix)]
    {
        let mut signal_interrupt = signal(SignalKind::interrupt())?;
        let mut signal_quit = signal(SignalKind::quit())?;
        let mut signal_terminate = signal(SignalKind::terminate())?;

        tokio::select! {
            _ = signal_interrupt.recv() => {},
            _ = signal_quit.recv() => {},
            _ = signal_terminate.recv() => {},
        }
    }
    #[cfg(windows)]
    {
        let mut ctrl_break = windows::ctrl_break()?;
        let mut ctrl_c = windows::ctrl_c()?;
        let mut ctrl_close = windows::ctrl_close()?;
        let mut ctrl_logoff = windows::ctrl_logoff()?;
        let mut ctrl_shutdown = windows::ctrl_shutdown()?;

        tokio::select! {
            _ = ctrl_break.recv() => {},
            _ = ctrl_c.recv() => {},
            _ = ctrl_close.recv() => {},
            _ = ctrl_logoff.recv() => {},
            _ = ctrl_shutdown.recv() => {},
        }
    }

    // A shutdown signal was received, now we need to let all of the spawned
    // tasks know and let them finish gracefully.
    info!("shutdown signal received");
    shutdown_sender
        .send(())
        .expect("shutdown broadcast could not be sent");

    // Wait for the server task to finish, which itself should wait for all of
    // its spawned tasks to finish.
    server
        .handle()
        .await
        .expect("server task did not finish to completion");

    Ok(())
}
