use std::io::{self, Read};

fn main() {
    let file = std::env::args_os().nth(1).expect("file to hold");
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1);
    }
    let _file = options.open(file).expect("holding the fixture file");
    println!("ready");
    let _ = io::stdin().read(&mut [0]);
}
