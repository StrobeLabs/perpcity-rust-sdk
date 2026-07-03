fn main() {
    for selector in [
        "0xed126f97",
        "0xfb30d03a",
        "0x0470009e",
        "0xb2c649db",
        "0x6f0f5899",
    ] {
        println!(
            "{} {:?}",
            selector,
            perpcity_sdk::errors::decode::decode_revert_data(selector)
        );
    }
}
