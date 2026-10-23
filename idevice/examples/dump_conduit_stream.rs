//! Dump the raw streaming_zip_conduit stream for a local `.app` directory to stdout.
//!
//! Used by CI to compare the Rust byte format against pymobiledevice3's reference, which is proven
//! to install on the watch. Run: `cargo run --example dump_conduit_stream --features
//! streaming_zip_conduit -- path/to/My.app > stream.bin`.

#[cfg(feature = "streaming_zip_conduit")]
fn main() {
    use std::io::Write;
    let app = std::env::args()
        .nth(1)
        .expect("usage: dump_conduit_stream <app_dir>");
    let bytes = idevice::services::streaming_zip_conduit::app_stream_bytes(std::path::Path::new(
        &app,
    ))
    .expect("failed to build stream");
    std::io::stdout().write_all(&bytes).expect("write failed");
}

#[cfg(not(feature = "streaming_zip_conduit"))]
fn main() {
    eprintln!("build with --features streaming_zip_conduit");
    std::process::exit(2);
}
