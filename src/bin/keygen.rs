//! Maintainer-only license key generator. NOT included in release zips.

fn main() {
    println!("{}", gguf_janitor::license::generate_pro_key());
}
