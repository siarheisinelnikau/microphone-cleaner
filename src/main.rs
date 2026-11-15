#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    #[cfg(windows)]
    if let Err(error) = microphone_cleaner::ui::run() {
        microphone_cleaner::ui::show_startup_error(&error);
    }
    #[cfg(not(windows))]
    eprintln!("Microphone Cleaner currently supports Windows only.");
}
