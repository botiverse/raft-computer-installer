#[tokio::main]
async fn main() {
    let code = match raft_computer_installer::cli::run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Installation: {error}");
            println!("The installation could not finish.");
            1
        }
    };
    std::process::exit(i32::from(code));
}
