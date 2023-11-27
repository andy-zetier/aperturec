use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod latency;
mod list;
mod plot;

#[derive(Parser)]
#[command(name = "cargo")]
#[command(bin_name = "cargo")]
enum CargoCli {
    TraceQueues(Args),
}

#[derive(clap::Args)]
#[command(author, version, about)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List all queues available for tracing in a given crate
    List {
        /// Manifest file for the crate of interest.  Defaults to the current crate if none
        /// provided
        #[arg(short, long)]
        manifest_path: Option<PathBuf>,
    },
    /// Analyze latency of a given queue
    AnalyzeLatency {
        /// CSV files where latency data is stored
        csv_files: Vec<PathBuf>,
    },
    /// Plot a given time sequence CSV file
    PlotCsv {
        /// CSV file to plot
        #[arg(short, long)]
        input: PathBuf,

        /// Output PNG file
        #[arg(short, long)]
        output: PathBuf,

        /// Caption for the plot
        #[arg(short, long)]
        caption: String,
    },
}

fn main() -> Result<()> {
    let CargoCli::TraceQueues(Args { command }) = CargoCli::parse();
    Ok(match command {
        Commands::List { manifest_path } => list::list_queues(manifest_path),
        Commands::AnalyzeLatency { csv_files } => latency::analyze_latency(csv_files),
        Commands::PlotCsv {
            input,
            output,
            caption,
        } => plot::plot_csv(input, output, caption),
    }?)
}
