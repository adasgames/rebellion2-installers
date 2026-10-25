#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    let executable = std::env::current_exe().expect("fixture executable path");
    std::fs::write(
        executable.parent().unwrap().join("game-started"),
        b"started",
    )
    .expect("recording the fixture game launch");
}
