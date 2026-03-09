fn main() {
    let version = std::process::Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
        .trim()
        .to_string();

    if !version.is_empty() {
        println!("cargo:rustc-env=WERMA_GIT_VERSION={version}");
    }
    println!("cargo:rerun-if-changed=.git/HEAD");
}
