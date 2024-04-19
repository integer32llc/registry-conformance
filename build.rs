use std::env;

fn main() {
    let target = env::var("TARGET").expect("TARGET must be set");
    println!("cargo::rustc-env=CURRENT_TARGET={target}");
}
