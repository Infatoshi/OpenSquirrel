mod app;
mod assets;
mod changes;
mod config;
mod daemon;
mod daemon_client;
mod hooks;
mod runtime;
mod theme;
mod worktree;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--daemon") {
        daemon::run();
        return;
    }

    if args.iter().any(|a| a == "--ensure") {
        daemon::ensure();
        return;
    }

    // Default: normal GUI mode
    app::run();
}
