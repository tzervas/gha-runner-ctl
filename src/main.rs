//! Binary entrypoint for gha-runner-ctl.
//! Delegates execution to the library module.

fn main() {
    gha_runner_ctl::prevent_raw_token_args();
    if let Err(e) = gha_runner_ctl::run() {
        eprintln!("gha-runner-ctl: {}", gha_runner_ctl::redact(&e));
        std::process::exit(1);
    }
}
