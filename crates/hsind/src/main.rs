mod app;
mod config;
mod crypto;
mod db;
mod error;
mod model;
mod paths;
mod proxy;
mod rpc;
mod service;

use clap::{Parser, Subcommand};
use error::Result;
use paths::{InstanceGuard, Paths};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "hsind", version, about = "hsin provider daemon")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground.
    Run,
    /// Install and control the per-user daemon service.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
}

#[derive(Subcommand)]
enum ServiceCommand {
    Install {
        #[arg(long)]
        start: bool,
    },
    Uninstall {
        #[arg(long)]
        purge: bool,
    },
    Start,
    Stop,
    Restart,
    Status,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("hsind=info")),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Err(error) = execute(Cli::parse()).await {
        tracing::error!(code=error.code(),%error,"hsind failed");
        std::process::exit(1);
    }
}

async fn execute(cli: Cli) -> Result<()> {
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run().await,
        Command::Service { command } => {
            match command {
                ServiceCommand::Install { start } => service::install(start)?,
                ServiceCommand::Uninstall { purge } => service::uninstall(purge)?,
                ServiceCommand::Start => service::start()?,
                ServiceCommand::Stop => service::stop()?,
                ServiceCommand::Restart => service::restart()?,
                ServiceCommand::Status => {
                    let running = service::status()?;
                    println!("{}", if running { "running" } else { "stopped" });
                    if !running {
                        std::process::exit(3);
                    }
                }
            }
            Ok(())
        }
    }
}

async fn run() -> Result<()> {
    let paths = Paths::discover();
    paths.prepare()?;
    let _instance = InstanceGuard::acquire(&paths.lock)?;
    let app = app::App::open(&paths)?;
    tolerate_locked("recover operations", app.recover_operations())?;
    tolerate_locked("initialize providers", app.initialize_providers())?;
    tolerate_locked(
        "reconcile client auth configuration",
        app.reconcile_client_auth_configuration(),
    )?;
    tolerate_locked(
        "reconcile proxy configurations",
        app.reconcile_proxy_configurations(),
    )?;
    let mut rpc = tokio::spawn(rpc::serve(app.clone()));
    tokio::task::yield_now().await;
    let proxy = tokio::spawn(proxy::serve(app.clone()));
    tracing::info!("hsind started");
    let rpc_result = tokio::select! {
        signal=shutdown_signal()=>{if let Err(error)=signal{tracing::warn!(%error,"failed to listen for shutdown signal");} None},
        ()=app.wait_shutdown()=>{None},
        result=&mut rpc=>{Some(result)},
    };
    let rpc_stopped_unexpectedly = rpc_result.is_some() && !app.is_shutdown_requested();
    app.notify_shutdown();
    let rpc_result = match rpc_result {
        Some(result) => result,
        None => rpc.await,
    };
    if rpc_stopped_unexpectedly {
        match rpc_result {
            Ok(Err(error)) => return Err(error),
            Err(error) => return Err(error::DaemonError::Internal(error.to_string())),
            Ok(Ok(())) => {
                return Err(error::DaemonError::Protocol(
                    "IPC server stopped unexpectedly".into(),
                ));
            }
        }
    }
    match rpc_result {
        Ok(Err(error)) => tracing::error!(%error,"IPC server stopped"),
        Err(error) => tracing::error!(%error,"IPC task failed"),
        Ok(Ok(())) => {}
    }
    match proxy.await {
        Ok(Err(error)) => tracing::warn!(%error,"proxy server stopped"),
        Err(error) => tracing::warn!(%error,"proxy task failed"),
        _ => {}
    }
    #[cfg(unix)]
    if let hsin_ipc::IpcEndpoint::Filesystem(path) = hsin_ipc::default_endpoint() {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

/// Startup reconciliation needs the master key and reads state the daemon does
/// not control: a managed client config can name an invalid URL, a stored proxy
/// port can be taken. Neither may take the daemon down, because the CLI is the
/// only way to fix any of it and the CLI needs the IPC socket. Failures that are
/// not about bad external input still stop the daemon.
fn tolerate_locked(step: &'static str, result: Result<()>) -> Result<()> {
    match result {
        Err(error::DaemonError::Locked) => {
            tracing::warn!(
                step,
                "skipped startup reconciliation; the key store is locked"
            );
            Ok(())
        }
        Err(error::DaemonError::Invalid(reason)) => {
            tracing::warn!(
                step,
                %reason,
                "skipped startup reconciliation; fix the reported configuration and restart"
            );
            Ok(())
        }
        other => other,
    }
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_locked_key_store_never_aborts_startup() {
        // Aborting here would crash-loop the service and leave no IPC socket,
        // so `hsin security import-recovery-key` could never reach the daemon.
        assert!(tolerate_locked("step", Err(error::DaemonError::Locked)).is_ok());
    }

    #[test]
    fn invalid_managed_configuration_never_aborts_startup() {
        // A client config naming a non-HTTPS provider URL is rejected during
        // reconciliation. Aborting would crash-loop the daemon and leave the
        // user no way in to correct the very config that caused it.
        assert!(
            tolerate_locked(
                "step",
                Err(error::DaemonError::Invalid("bad provider URL".into()))
            )
            .is_ok()
        );
    }

    #[test]
    fn other_startup_failures_still_stop_the_daemon() {
        let error = tolerate_locked("step", Err(error::DaemonError::Crypto)).unwrap_err();
        assert_eq!(error.code(), error::DaemonError::Crypto.code());
    }
}
