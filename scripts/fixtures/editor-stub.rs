#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    let executable = std::env::current_exe().expect("fixture executable path");
    let directory = executable.parent().unwrap();
    std::fs::write(
        directory.join("editor-started"),
        std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .as_bytes(),
    )
    .expect("recording the fixture editor launch");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if directory.join("editor-confirm").is_file() {
            std::fs::write(directory.join("editor-alive"), b"alive")
                .expect("recording editor survival after launcher exit");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
