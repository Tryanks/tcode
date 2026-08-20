//! Live microphone dictation smoke test: `cargo run -p tcode-voice --example
//! mic_dictation -- zh_CN [seconds]`. Prints every event with a timestamp.
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let locale = args.next().unwrap_or_else(|| "zh_CN".into());
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);
    let start = Instant::now();
    let session = tcode_voice::start(
        &locale,
        Box::new(move |event| println!("[{:6.2}s] {event:?}", start.elapsed().as_secs_f32())),
    )
    .expect("failed to start dictation");
    std::thread::sleep(Duration::from_secs(seconds));
    println!("[{:6.2}s] calling stop()", start.elapsed().as_secs_f32());
    session.stop();
    std::thread::sleep(Duration::from_secs(3));
}
