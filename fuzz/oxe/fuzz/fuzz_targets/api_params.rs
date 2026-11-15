#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| oxe_fuzz::api_params(data));
