use clap::Parser;

fn main() {
    if let Err(err) = cdc_cli::run(cdc_cli::Cli::parse()) {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
