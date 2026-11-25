//! Disassemble pipeline shader blobs dumped from a live title run (`TOMBOX_PASSCAP_SHADERS`).
//!
//!     cargo run --release -p tombox-xshader --bin xdisasm -- <dir>/<pso>_sub1.bin
//!
//! Console (RDNA2) blobs do not disassemble as DXBC/DXIL; the error is printed for those and the
//! translated PC shader at the same stage is what answers "which registers does this shader read".

fn main() {
    let mut bad = 0;
    for p in std::env::args().skip(1) {
        let b = match std::fs::read(&p) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{p}: {e}");
                bad += 1;
                continue;
            }
        };
        match tombox_xshader::dxc::disassemble(&b) {
            Ok(t) => println!("==== {p} ({} bytes) ====\n{t}", b.len()),
            Err(e) => {
                println!("{p} ({} bytes): {e}", b.len());
                bad += 1;
            }
        }
    }
    if bad != 0 {
        std::process::exit(1);
    }
}
