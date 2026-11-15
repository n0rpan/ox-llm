#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| oxe_fuzz::http_access(data));
