#![windows_subsystem = "windows"]

mod control;
mod devices;
mod ui;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Elevated helper: the GUI relaunches itself with this to run pnputil as admin.
    if args.len() == 4 && args[1] == "--apply" {
        std::process::exit(control::apply_from_file(&args[2], &args[3]));
    }

    // Diagnostics: write what the scanner sees to a text file.
    if args.len() == 3 && args[1] == "--dump" {
        let t0 = std::time::Instant::now();
        let bt = devices::scan(false);
        let t1 = t0.elapsed();
        let full = devices::scan(true);
        let t2 = t0.elapsed() - t1;
        let report = format!("{}scan: bluetooth {:?}, with receiver check {:?}\n", full.describe(), t1, t2);
        drop(bt);
        let _ = std::fs::write(&args[2], report);
        return;
    }

    ui::run();
}
