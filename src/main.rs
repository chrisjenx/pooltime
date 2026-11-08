fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    match pooltime::cli::run(args) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}
