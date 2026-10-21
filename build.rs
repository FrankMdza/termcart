fn main() {
    println!("cargo:rerun-if-changed=src/libretro/log_shim.c");
    cc::Build::new()
        .file("src/libretro/log_shim.c")
        .compile("termcart_log_shim");
}
