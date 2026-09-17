use std::fs;
use std::io::Write;
use std::path::Path;

use crate::host::Host;

pub fn write(host: &mut impl Host, output: Option<&Path>, text: &str) -> bool {
    let result = match output {
        Some(path) => {
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                match host.current_dir() {
                    Ok(current_dir) => current_dir.join(path),
                    Err(error) => {
                        let _ = writeln!(host.error(), "Error getting current directory: {error}");
                        host.exit(1);
                        return false;
                    }
                }
            };
            path.parent()
                .map_or(Ok(()), fs::create_dir_all)
                .and_then(|()| fs::write(path, text))
        }
        None => host.output().write_all(text.as_bytes()),
    };
    if let Err(error) = result {
        let destination = output.map_or_else(|| "stdout".to_string(), |path| path.display().to_string());
        let _ = writeln!(host.error(), "Error writing output to {destination}: {error}");
        host.exit(1);
        return false;
    }
    true
}
