//! Binary entrypoint for gha-runner-ctl.
//! Delegates execution to the library module.

fn main() {
    gha_runner_ctl::prevent_raw_token_args();
    gha_runner_ctl::refuse_root_unless_allowed();
    if let Err(e) = gha_runner_ctl::run() {
        let msg = gha_runner_ctl::redact(&e);
        eprintln!("gha-runner-ctl: {msg}");
        // Until the stack is stable: dump context on failure.
        // GHA_DEBUG=1 always; GHA_DEBUG_ON_ERR=1 (default) only on error.
        gha_runner_ctl::debug_dump_on_error(&msg);
        std::process::exit(1);
    }
}
