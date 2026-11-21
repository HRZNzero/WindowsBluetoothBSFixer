// Embeds an application manifest: Common Controls v6 (themed controls, list-view groups),
// per-monitor-v2 DPI awareness and UTF-8. No resource compiler needed.
fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest::embed_manifest(embed_manifest::new_manifest("WindowsBluetoothBSFixer"))
            .expect("unable to embed manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
