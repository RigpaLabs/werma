fn main() {
    let output = std::process::Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output();
    let version = match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => "unknown".to_string(),
    };
    println!("cargo:rustc-env=WERMA_GIT_VERSION={}", version);
    println!("cargo:rerun-if-changed=.git/HEAD");
}
