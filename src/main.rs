//! `linf` — local-infra.
//!
//! Argument parsing, dispatch and exit codes all live in `cli`; with no
//! subcommand it opens the TUI (CLI-001).

fn main() -> std::process::ExitCode {
    local_infra::cli::main()
}
