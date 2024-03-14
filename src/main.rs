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

#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};
#[cfg(windows)]
use tokio::signal::windows;
use tracing::info;
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::{fmt, prelude::*};

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    // Print spans and events to STDOUT in a nice format.
    let fmt_layer = fmt::layer();

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

    // TODO: Start the server.

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

    // TODO: Send shutdown broadcast, await handles.

    Ok(())
}
