mod cli;

fn main() -> std::process::ExitCode {
    match cli::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            cli::print_error(&error);
            std::process::ExitCode::FAILURE
        }
    }
}
