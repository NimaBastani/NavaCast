fn main() {
    if let Err(err) = navacast::daemon::run() {
        eprintln!("navad: {err}");
        std::process::exit(1);
    }
}
