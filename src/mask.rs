//! Secret masking (§9.3). Every `env_file` value of 6 or more characters is
//! replaced with `***` before output reaches the tmux pane, and so before any
//! TUI sees it. The streaming
//! masker holds back only the shortest tail that could still be the start of a
//! secret, so a secret split across two chunks is still caught without delaying
//! ordinary output.

use regex::bytes::Regex;

const MIN_SECRET_LEN: usize = 6;
const MASK: &[u8] = b"***";

#[derive(Debug, Clone)]
pub struct Masker {
    secrets: Vec<Vec<u8>>,
    re: Option<Regex>,
    carry: Vec<u8>,
}

impl Masker {
    pub fn new<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut secrets: Vec<Vec<u8>> =
            values.into_iter().map(|v| v.as_ref().as_bytes().to_vec()).filter(|v| v.len() >= MIN_SECRET_LEN).collect();
        // Longest first so a secret that contains another is masked whole.
        secrets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        secrets.dedup();
        let re = (!secrets.is_empty()).then(|| {
            let alts: Vec<String> = secrets.iter().map(|s| regex::escape(&String::from_utf8_lossy(s))).collect();
            Regex::new(&alts.join("|")).expect("escaped alternation is valid")
        });
        Masker { secrets, re, carry: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// Masks a complete buffer (no streaming state).
    pub fn mask_all(&self, data: &[u8]) -> Vec<u8> {
        match &self.re {
            Some(re) => re.replace_all(data, MASK).into_owned(),
            None => data.to_vec(),
        }
    }

    /// Masks the next chunk of a stream. Returns the bytes that are safe to emit.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.re.is_none() {
            return chunk.to_vec();
        }
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        let masked = self.mask_all(&buf);
        let hold = self.partial_suffix_len(&masked);
        let split = masked.len() - hold;
        self.carry = masked[split..].to_vec();
        masked[..split].to_vec()
    }

    /// Emits whatever is still held back (end of stream).
    pub fn flush(&mut self) -> Vec<u8> {
        let rest = std::mem::take(&mut self.carry);
        self.mask_all(&rest)
    }

    /// Length of the longest suffix of `buf` that is a proper prefix of a secret.
    fn partial_suffix_len(&self, buf: &[u8]) -> usize {
        let mut best = 0;
        for s in &self.secrets {
            let max = (s.len() - 1).min(buf.len());
            for len in (best + 1..=max).rev() {
                if buf[buf.len() - len..] == s[..len] {
                    best = len;
                    break;
                }
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(m: &mut Masker, chunks: &[&str]) -> String {
        let mut out = Vec::new();
        for c in chunks {
            out.extend(m.push(c.as_bytes()));
        }
        out.extend(m.flush());
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn masks_whole_secret() {
        let m = Masker::new(["s3cr3t-token"]);
        assert_eq!(m.mask_all(b"token=s3cr3t-token;"), b"token=***;");
    }

    #[test]
    fn ignores_short_values() {
        let m = Masker::new(["abc", "12345"]);
        assert!(m.is_empty());
        assert_eq!(m.mask_all(b"abc 12345"), b"abc 12345");
    }

    #[test]
    fn catches_secret_split_across_chunks() {
        let mut m = Masker::new(["hunter22-password"]);
        assert_eq!(stream(&mut m, &["login hunt", "er22-pass", "word ok\n"]), "login *** ok\n");
    }

    #[test]
    fn split_at_every_position() {
        let secret = "abcdef123456";
        let text = format!("x{secret}y{secret}");
        for i in 0..text.len() {
            let mut m = Masker::new([secret]);
            let out = stream(&mut m, &[&text[..i], &text[i..]]);
            assert_eq!(out, "x***y***", "split at {i}");
        }
    }

    #[test]
    fn byte_by_byte() {
        let mut m = Masker::new(["password1"]);
        let text = "a password1 b passwor c";
        let chunks: Vec<String> = text.chars().map(|c| c.to_string()).collect();
        let refs: Vec<&str> = chunks.iter().map(|s| s.as_str()).collect();
        assert_eq!(stream(&mut m, &refs), "a *** b passwor c");
    }

    #[test]
    fn does_not_hold_unrelated_output() {
        let mut m = Masker::new(["supersecret"]);
        assert_eq!(m.push(b"hello world\n"), b"hello world\n");
        // "su" could be the start of the secret, so it is held.
        assert_eq!(m.push(b"tail su"), b"tail ");
        assert_eq!(m.flush(), b"su");
    }

    #[test]
    fn longest_secret_wins() {
        let m = Masker::new(["abcdef", "abcdefghij"]);
        assert_eq!(m.mask_all(b"[abcdefghij]"), b"[***]");
    }

    #[test]
    fn regex_metacharacters_are_literal() {
        let m = Masker::new(["a.b*c+d?"]);
        assert_eq!(m.mask_all(b"a.b*c+d? axbbcd"), b"*** axbbcd");
    }
}
