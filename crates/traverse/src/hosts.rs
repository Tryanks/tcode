//! `hosts.json`: the machines this device has paired with. Field-level
//! updates from the UI and the transport, possibly from several app processes
//! sharing one profile, serialize on a lock file; readers see either complete
//! version via rename.
use std::{fs, io, path::Path};

use tcode_client::pairing::PairedHost;

pub fn load_hosts(data_dir: &Path) -> io::Result<Vec<PairedHost>> {
    match fs::read(data_dir.join("hosts.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

pub fn save_hosts(data_dir: &Path, hosts: &[PairedHost]) -> io::Result<()> {
    let _lock = hosts_lock(data_dir)?;
    write_hosts(data_dir, hosts)
}

pub fn update_hosts(data_dir: &Path, update: impl FnOnce(&mut Vec<PairedHost>)) -> io::Result<()> {
    let _lock = hosts_lock(data_dir)?;
    let mut hosts = load_hosts(data_dir)?;
    let before = hosts.clone();
    update(&mut hosts);
    if hosts != before {
        write_hosts(data_dir, &hosts)?;
    }
    Ok(())
}

fn hosts_lock(data_dir: &Path) -> io::Result<fs::File> {
    fs::create_dir_all(data_dir)?;
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(data_dir.join("hosts.lock"))?;
    file.lock()?;
    Ok(file)
}

fn write_hosts(data_dir: &Path, hosts: &[PairedHost]) -> io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let bytes = serde_json::to_vec_pretty(hosts).map_err(io::Error::other)?;
    crate::identity::write_private(&data_dir.join("hosts.json"), &bytes)
}
