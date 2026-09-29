fn main() {
    #[cfg(feature = "cbench")]
    {
        use dacode_json::cbench::{Padded, SimdJson, YyJson};
        println!("yyjson    version : {}", YyJson::version());
        println!("simdjson  version : {}", SimdJson::version());
        println!("simdjson  kernel  : {}", SimdJson::implementation());
        println!("simdjson  padding : {}", SimdJson::padding());

        let src = br#"{"a":1,"b":[1,2,3],"c":"hi"}"#;
        println!("\nyyjson parse val count = {}", YyJson::parse(src));
        println!("yyjson validate        = {}", YyJson::validate(src));
        println!("yyjson roundtrip len   = {}", YyJson::roundtrip(src));

        let arr = br#"[{"score":10},{"score":32}]"#;
        println!(
            "yyjson sum_field score = {}",
            YyJson::sum_field(arr, "score")
        );

        let p = Padded::new(arr);
        println!(
            "simdjson sum_field     = {}",
            SimdJson::sum_field(&p, "score")
        );
        println!("simdjson ondemand n    = {}", SimdJson::parse_ondemand(&p));
        println!("simdjson validate      = {}", SimdJson::validate(&p));
    }
    #[cfg(not(feature = "cbench"))]
    println!("build with --features cbench");
}
