#![no_main]
use byblos::content::Lexer;
use libfuzzer_sys::fuzz_target;

// The lexer must stop within len + 1 tokens (Go's FuzzLexer): a token that consumes no byte
// would loop a caller forever. An error or the end of the input ends the loop.
fuzz_target!(|data: &[u8]| {
    let mut l = Lexer::new(data);
    for _ in 0..=data.len() + 1 {
        if !matches!(l.next_token(), Ok(Some(_))) {
            return;
        }
    }
    panic!("lexer produced more tokens than input bytes");
});
