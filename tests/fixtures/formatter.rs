use std::env;
use std::fs;
use std::io::{self, Read, Write};

fn main() {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("identity");
    if mode == "fail" {
        eprintln!("fixture formatter failed");
        std::process::exit(42);
    }
    // Successful exits without consuming stdin exercise BrokenPipe precedence.
    match mode {
        "early-empty" => return,
        "early-invalid" => {
            io::stdout().write_all(&[255]).unwrap();
            return;
        }
        "early-valid" => {
            io::stdout().write_all(b"formatted").unwrap();
            return;
        }
        _ => {}
    }
    if mode == "flood" {
        io::stdout().write_all(&vec![b'P'; 131_072]).unwrap();
        io::stdout().flush().unwrap();
        io::stderr().write_all(&vec![b'D'; 32_768]).unwrap();
        io::stderr().flush().unwrap();
    }
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input).unwrap();
    match mode {
        "identity" | "flood" => {}
        "upper" => input.make_ascii_uppercase(),
        "select-fail" => {
            if args[2] == "b.rs" {
                eprintln!("fixture formatter failed");
                std::process::exit(42);
            }
            input.make_ascii_uppercase();
        }
        "empty" => input.clear(),
        "invalid" => input = vec![255],
        "context" => {
            let source = String::from_utf8(input).unwrap();
            input = format!(
                "{source}|{}|{}|{}|{}",
                args[2],
                args[3],
                args[4],
                fs::exists("root.marker").unwrap()
            )
            .into_bytes();
        }
        "mutate" => fs::write(&args[2], "external change").unwrap(),
        "directory" => {
            fs::remove_file(&args[2]).unwrap();
            fs::create_dir(&args[2]).unwrap();
        }
        "remove" => fs::remove_file(&args[2]).unwrap(),
        "rename" => fs::rename(&args[2], "renamed.rs").unwrap(),
        _ => {
            eprintln!("unknown fixture mode");
            std::process::exit(64);
        }
    }
    io::stdout().write_all(&input).unwrap();
}
