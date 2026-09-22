fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).map(|s| s.as_str()).unwrap_or("main.hard");
    let src = std::fs::read_to_string(path).unwrap();
    match hs_compiler::compile_to_cpp(&src, path.to_string()) {
        Ok(c) => print!("{}", c),
        Err(d) => for x in &d {
            println!("{:?}", x);
        },
    }
}