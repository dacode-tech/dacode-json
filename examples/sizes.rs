//! Buffer sizes for each jsonflat configuration, against JSON and rkyv.
use dacode_json::flat::{Builder, Intern};
use dacode_json::strict::StrictParser;
use dacode_json::corpus;

fn main() {
    for (name, json) in corpus::suite(1 << 20, 0x2C0) {
        let mut p = StrictParser::new();
        let doc = p.parse(json.as_bytes()).expect("valid");
        let j = json.len() as f64;
        print!("{name:12} json={:>9}", json.len());
        for (label, b) in [
            ("none", Builder::new().intern(Intern::None)),
            ("keys", Builder::new().intern(Intern::Keys)),
            ("all",  Builder::new().intern(Intern::All)),
        ] {
            let buf = b.build(doc).expect("build");
            print!("  {label}={:.2}x", buf.len() as f64 / j);
        }
        println!();
    }
}
