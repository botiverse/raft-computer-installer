#[tokio::main]
async fn main() {
    let code = match raft_computer_installer::cli::run().await {
        Ok(code) => code,
        Err(error) => {
            raft_computer_installer::cli::preserve_top_level_failure(&error);
            eprintln!("{}", raft_computer_installer::cli::failure_line());
            1
        }
    };
    std::process::exit(i32::from(code));
}
