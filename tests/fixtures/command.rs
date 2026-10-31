use std::fs::OpenOptions;
use std::io::{Read, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args[0].as_str() {
        "record" => {
            let mut file = OpenOptions::new().create(true).append(true).open(&args[1]).unwrap();
            writeln!(file, "{}", args[2]).unwrap();
            println!("stdout: {}", args[2]);
            eprintln!("stderr: {}", args[2]);
            std::process::exit(args[3].parse().unwrap());
        }
        "echo" => {
            println!("{}", std::env::current_dir().unwrap().display());
            for arg in &args[1..] {
                println!("{arg}");
            }
        }
        "stdin" => {
            let mut bytes = Vec::new();
            std::io::stdin().read_to_end(&mut bytes).unwrap();
            assert!(bytes.is_empty());
        }
        "large" => {
            print!("{}", "é".repeat(17_000));
            std::io::stderr().write_all(&[0xff; 17_000]).unwrap();
        }
        _ => panic!("unknown fixture operation"),
    }
}
