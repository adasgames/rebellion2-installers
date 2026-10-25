use std::io::{Read, Write};

fn main() {
    let executable = std::env::current_exe().unwrap();
    let helper = std::process::Command::new(
        executable
            .parent()
            .unwrap()
            .join("rebellion2-update-helper.exe"),
    )
    .spawn()
    .unwrap();
    println!("{}", helper.id());
    std::io::stdout().flush().unwrap();
    let _ = std::io::stdin().read(&mut [0_u8]);
}
