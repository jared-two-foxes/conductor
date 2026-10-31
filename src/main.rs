use clap::{Parser, Subcommand};
use conductor::{WorkflowRequest, WorkflowResult, run};
use std::fs;
use std::io::{self, Read};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    version,
    about = "Run commands sequentially, stopping at the first failure"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Run {
        #[arg(long, value_name = "FILE_OR_DASH")]
        request: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let Command::Run { request } = cli.command;
    let bytes = if request == "-" {
        let mut bytes = Vec::new();
        io::stdin().read_to_end(&mut bytes).map(|_| bytes)
    } else {
        fs::read(&request)
    };
    let outcome = match bytes {
        Ok(bytes) => match serde_json::from_slice::<WorkflowRequest>(&bytes) {
            Ok(request) => run(&request),
            Err(error) => {
                WorkflowResult::blocked("input", format!("Invalid request JSON: {error}"))
            }
        },
        Err(error) => WorkflowResult::blocked("input", format!("Cannot read request: {error}")),
    };
    let exit = ExitCode::from(outcome.status.exit_code());
    println!(
        "{}",
        serde_json::to_string(&outcome).expect("WorkflowResult is serializable")
    );
    exit
}
