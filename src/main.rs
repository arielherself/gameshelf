mod cli;
mod config;
mod launch;
mod library;
mod lockfile;
mod names;
mod runtime;
mod session;
mod steam;
mod tui;
mod util;

fn main() {
    match cli::main() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("gameshelf: {e:#}");
            std::process::exit(1);
        }
    }
}
