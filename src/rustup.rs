use std::process::Command;

/// Get the default rustup toolchain or `stable` if the default can't be determined
pub fn default_toolchain() -> String {
    Command::new("rustup")
        .arg("default")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|stdout| {
            stdout
                .split_once(' ')
                .map(|(toolchain, _)| toolchain.to_owned())
        })
        .unwrap_or_else(|| "stable".to_owned())
}
