#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    let executable = std::env::current_exe().expect("fixture executable path");
    let directory = executable.parent().unwrap();
    let working_directory = std::env::current_dir().expect("fixture working directory");
    let start_page = std::env::args()
        .skip_while(|argument| argument != "-startPage")
        .nth(1)
        .unwrap_or_default();
    std::fs::write(
        directory.join("editor-started"),
        format!("{}\n{start_page}", working_directory.display()),
    )
    .expect("recording the fixture editor launch");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if directory.join("editor-confirm").is_file() {
            std::fs::write(directory.join("editor-alive"), b"alive")
                .expect("recording editor readiness");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
