#[tokio::main]
async fn main() {
    let code = match raft_computer_installer::cli::run().await {
        Ok(code) => code,
        Err(error) => raft_computer_installer::cli::report_top_level_failure(&error),
    };
    std::process::exit(i32::from(code));
}
