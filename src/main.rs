//! One GitHub Actions self-hosted runner controller (Podman) executable entrypoint.

fn main() {
    if let Err(e) = gha_runner_ctl::run() {
        eprintln!("gha-runner-ctl: {}", gha_runner_ctl::redact(&e));
        std::process::exit(1);
    }
}
