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
        "report" => {
            let report = format!(
                "{{\"padding\":\"{}\",\"findings\":[{{\"message\":\"failure details at end\"}}]}}",
                "x".repeat(100_000)
            );
            std::io::stdout().write_all(report.as_bytes()).unwrap();
            std::io::stderr().write_all(&vec![b'x'; 128 * 1024]).unwrap();
            std::io::stderr().write_all(b"\nFAILURE: expected 42, got 7\n").unwrap();
            std::process::exit(2);
        }
        "repeat" => {
            std::io::stdout().write_all(args[1].repeat(args[2].parse().unwrap()).as_bytes()).unwrap();
        }
        "condition" => {
            let raw_code = std::env::var("CONDUCTOR_COMMAND_EXIT_CODE").unwrap();
            let stdout = std::fs::read(std::env::var("CONDUCTOR_COMMAND_STDOUT_PATH").unwrap()).unwrap();
            let stderr = std::fs::read(std::env::var("CONDUCTOR_COMMAND_STDERR_PATH").unwrap()).unwrap();
            assert!(!stderr.is_empty());
            assert_eq!(std::env::var("CONDUCTOR_COMMAND_NAME").unwrap(), args[1]);
            assert_eq!(std::env::var("CONDUCTOR_COMMAND_INDEX").unwrap(), args[2]);
            println!("evaluated raw exit {raw_code}");
            let accepted = match args[3].as_str() {
                "output-contains" => stdout.windows(args[4].len()).any(|part| part == args[4].as_bytes()),
                "output-excludes" => !stdout.windows(args[4].len()).any(|part| part == args[4].as_bytes()),
                "directory-absent" => raw_code == "1" && !std::path::Path::new(&args[4]).exists(),
                _ => panic!("unknown condition"),
            };
            if !accepted { eprintln!("condition rejected command output/state"); std::process::exit(7); }
        }
        "create-directory" => { std::fs::create_dir(&args[1]).unwrap(); }
        "block-next-log" | "remove-current-stdout" => {
            let entries: Vec<_> = std::fs::read_dir(&args[1]).unwrap().collect();
            assert_eq!(entries.len(), 1);
            let logs = entries.into_iter().next().unwrap().unwrap().path();
            if args[0] == "block-next-log" {
                std::fs::write(logs.join("000001.stderr"), "do not overwrite").unwrap();
            } else {
                std::fs::remove_file(logs.join("000000.stdout")).unwrap();
            }
        }
        _ => panic!("unknown fixture operation"),
    }
}
