fn main() -> std::process::ExitCode {
    ccstatusline_native::run(std::env::args_os().skip(1))
}
