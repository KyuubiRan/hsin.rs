mod app;
mod bootstrap;
mod cli;
mod i18n;
mod rpc;
mod tui;
mod updater;

use std::process::ExitCode;

use hsin_core::Settings;
use serde_json::json;

#[tokio::main]
async fn main() -> ExitCode {
    let args = std::env::args_os().collect::<Vec<_>>();
    let language = match cli::language_override(&args) {
        Some(language) => Some(language),
        None => configured_language().await,
    };
    let mut i18n = i18n::I18n::new(language.as_deref());
    let cli = cli::Cli::parse_localized_from(args, &i18n);

    match app::run(cli, &mut i18n).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}: {}", i18n.text("error"), i18n.error_message(&error));
            ExitCode::FAILURE
        }
    }
}

async fn configured_language() -> Option<String> {
    let client = rpc::DaemonClient::connect().await.ok()?;
    let settings: Settings = client.call("settings.get", &json!({})).await.ok()?;
    Some(settings.language)
}
