use std::{env, process, sync::mpsc};

use tcode_voice::DictationEvent;

fn main() {
    let mut args = env::args().skip(1);
    let (Some(path), Some(locale), None) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: file_dictation <audio-path> <locale>");
        process::exit(2);
    };

    let (sender, receiver) = mpsc::channel();
    // Held for its lifetime: dropping it would cancel the session.
    let _session = tcode_voice::start_file(
        &locale,
        &path,
        Box::new(move |event| {
            let _ = sender.send(event);
        }),
    )
    .unwrap_or_else(|error| {
        eprintln!("error: {error}");
        process::exit(1);
    });

    while let Ok(event) = receiver.recv() {
        match event {
            DictationEvent::Ready => println!("ready"),
            DictationEvent::Level(_) => {}
            DictationEvent::Volatile(text) => println!("volatile: {text}"),
            DictationEvent::Final(text) => println!("final: {text}"),
            DictationEvent::Error(error) => {
                eprintln!("error: {error}");
                process::exit(1);
            }
            DictationEvent::Ended => {
                println!("ended");
                break;
            }
        }
    }
}
