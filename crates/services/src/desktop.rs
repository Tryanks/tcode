use std::io;
use std::path::Path;

pub fn open_in_zed(cwd: &Path) -> io::Result<()> {
    let child = crate::process::command("zed").arg(cwd).spawn()?;
    drop(child);
    Ok(())
}
