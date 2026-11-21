#![windows_subsystem = "windows"]

mod control;
mod devices;
mod ui;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Elevated helper: the GUI relaunches itself with this to make admin-only changes.
    if args.len() == 4 && args[1] == "--apply" {
        std::process::exit(control::apply_from_file(&args[2], &args[3]));
    }

    // Diagnostics: write what the scanner sees (and how long it took) to a text file.
    if args.len() == 3 && args[1] == "--dump" {
        let t0 = std::time::Instant::now();
        let snap = devices::scan();
        let report = format!("{}scan took {:?}\n", snap.describe(), t0.elapsed());
        let _ = std::fs::write(&args[2], report);
        return;
    }

    ui::run();
}
