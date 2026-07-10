use std::process::Command;

fn main() {
    // commit hash —— 远端用 tar 部署（无 .git 目录）或机器没装 git 时，
    // Command::new("git") 会返回 Err(NotFound)/非零退出。别 unwrap 直接崩，
    // 退回占位串让编译继续（GIT_DESCRIBE 仅用于 clap 的 --version 展示）。
    let git_describe = Command::new("git")
        .arg("describe")
        .arg("--always")
        .arg("--dirty")
        .arg("--tags")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo::rustc-env=GIT_DESCRIBE={git_describe}");
}
