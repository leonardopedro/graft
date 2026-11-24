use clap::Parser;

fn main() {
    if let Err(err) = graft_workspace::cli::run(graft_workspace::cli::Cli::parse()) {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
