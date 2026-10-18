mod cli;

fn main() {
    if let Err(error) = cli::run() {
        cli::print_error(&error);
        std::process::exit(1);
    }
}
