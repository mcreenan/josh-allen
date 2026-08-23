#![forbid(unsafe_code)]

mod cli;
mod runner;

fn main() -> std::process::ExitCode {
    let arguments = std::env::args().collect::<Vec<_>>();
    match arguments.get(1).map(String::as_str) {
        Some("serve") => serve(&arguments),
        Some("run") => runner::run(&arguments),
        Some("help" | "--help" | "-h") => {
            cli::print_help(arguments.first().map_or("josh", String::as_str));
            std::process::ExitCode::SUCCESS
        }
        Some(command) => {
            eprintln!("josh: unknown command '{command}'");
            cli::print_help(arguments.first().map_or("josh", String::as_str));
            std::process::ExitCode::from(2)
        }
        None => {
            eprintln!("josh: a command is required");
            cli::print_help(arguments.first().map_or("josh", String::as_str));
            std::process::ExitCode::from(2)
        }
    }
}

fn serve(arguments: &[String]) -> std::process::ExitCode {
    if arguments.len() > 2 {
        eprintln!("josh: 'serve' does not accept arguments");
        return std::process::ExitCode::from(2);
    }
    match josh_host::run_connection(std::io::stdin(), std::io::stdout()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("josh: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
