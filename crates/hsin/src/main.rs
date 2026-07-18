mod app;
mod bootstrap;
mod cli;
mod i18n;
mod rpc;
mod tui;

use std::process::ExitCode;

use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    let mut i18n = i18n::I18n::new(cli.language.as_deref());

    match app::run(cli, &mut i18n).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}: {}", i18n.text("error"), i18n.error_message(&error));
            ExitCode::FAILURE
        }
    }
}
