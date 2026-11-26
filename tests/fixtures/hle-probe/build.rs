fn main() {
    println!("cargo:rustc-link-arg-cdylib=/DELAYLOAD:vc-probe-delayed.dll");
    println!("cargo:rustc-link-lib=delayimp");
}
