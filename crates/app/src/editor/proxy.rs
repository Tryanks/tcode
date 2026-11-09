//! `tcode ssh-proxy <machine-id> --data-dir <dir>`: the ProxyCommand Tcode's
//! ssh config gives Zed's ssh. It opens no connection of its own — the same
//! device key twice would have the relay drop one — but asks the running
//! Tcode that owns the data dir to carry its bytes, then copies stdin and
//! stdout both ways. A refusal is one line on stderr, which Zed shows.
use std::{
    io::{Read as _, Write as _},
    path::PathBuf,
};

use super::endpoint::LocalStream;

/// Run the proxy for `args`, the words after `ssh-proxy`; the process's
/// exit status.
pub(crate) fn run(args: &[String]) -> i32 {
    tcode_ui::settings::apply_locale(None);
    let mut machine = None;
    let mut data_dir = None;
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--data-dir" => data_dir = words.next().map(PathBuf::from),
            other if machine.is_none() => machine = Some(other.to_owned()),
            _ => {}
        }
    }
    let Some(machine) = machine else {
        eprintln!("usage: tcode ssh-proxy <machine-id> [--data-dir <dir>]");
        return 2;
    };
    let data_dir = match data_dir.map_or_else(tcode_services::store::data_dir, Ok) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("{error}");
            return 255;
        }
    };
    let mut stream = match LocalStream::connect(&data_dir) {
        Ok(stream) => stream,
        Err(_) => {
            eprintln!("{}", tcode_ui::tr!("editor.zed.proxy_no_app"));
            return 255;
        }
    };
    if stream.write_all(format!("{machine}\n").as_bytes()).is_err() {
        eprintln!("{}", tcode_ui::tr!("editor.zed.proxy_no_app"));
        return 255;
    }
    let mut reply = Vec::new();
    let mut byte = [0];
    loop {
        match stream.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => break,
            Ok(1) if reply.len() < 4096 => reply.push(byte[0]),
            _ => {
                eprintln!("{}", tcode_ui::tr!("editor.zed.proxy_no_app"));
                return 255;
            }
        }
    }
    let reply = String::from_utf8_lossy(&reply);
    if reply != "ok" {
        eprintln!("{}", reply.strip_prefix("refused ").unwrap_or(&reply));
        return 255;
    }
    let Ok(mut upstream) = stream.try_clone() else {
        return 255;
    };
    // Whichever direction ends first ends the proxy, and with it ssh's
    // connection.
    let _ = std::thread::Builder::new()
        .name("tcode-ssh-proxy-stdin".into())
        .spawn(move || {
            let _ = std::io::copy(&mut std::io::stdin().lock(), &mut upstream);
            upstream.shutdown();
            std::process::exit(0);
        });
    let mut stdout = std::io::stdout().lock();
    let mut buffer = vec![0; 32 * 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                if stdout.write_all(&buffer[..count]).is_err() || stdout.flush().is_err() {
                    break;
                }
            }
        }
    }
    0
}
